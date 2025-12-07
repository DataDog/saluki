use std::fmt;
use std::marker::PhantomData;

use facet::{Facet, Field, NumericType, PrimitiveType, StructKind, StructType, TextualType, Type, UserType};
use facet_reflect::{Partial, ReflectError};
use facet_value::{DestructuredRef, Value};
use snafu::Snafu;
use tracing::trace;

/// Extracts the effective serialization name for a field, considering rename attributes.
fn get_serialization_name(field: &Field) -> &'static str {
    field
        .get_builtin_attr("rename")
        .and_then(|attr| attr.get_as::<&'static str>().copied())
        .unwrap_or(field.name)
}

/// Gets all possible lookup names for a field, in priority order.
/// Order: renamed name (if any) → original field name → aliases (in order)
fn get_field_lookup_names(field: &Field) -> impl Iterator<Item = &'static str> {
    // Start with renamed name if it exists, otherwise original field name
    let primary_name = field
        .get_builtin_attr("rename")
        .and_then(|attr| attr.get_as::<&'static str>().copied())
        .unwrap_or(field.name);

    // Collect aliases in order
    let aliases = field
        .attributes
        .iter()
        .filter(|attr| attr.ns.is_none() && attr.key == "alias")
        .filter_map(|attr| attr.get_as::<&'static str>().copied());

    // Chain: primary name, then original name (if different), then aliases
    std::iter::once(primary_name)
        .chain(if primary_name != field.name {
            Some(field.name)
        } else {
            None
        })
        .chain(aliases)
}

/// Searches for a field value in the object using all possible field names.
/// Tries names in priority order until a value is found.
fn find_field_value<'a>(field: &Field, obj: &'a facet_value::VObject) -> Option<&'a Value> {
    get_field_lookup_names(field).find_map(|name| obj.get(name))
}

/// Reasons why an enum value is invalid.
#[derive(Debug, Clone)]
pub enum InvalidEnumKind {
    /// The object representing the enum variant is empty.
    EmptyObject,

    /// The object has multiple keys when exactly one is expected.
    MultipleKeys,

    /// Failed to get the selected variant after selection.
    VariantSelectionFailed,

    /// A unit variant was given a non-null value.
    UnitVariantNotNull {
        /// The variant name.
        variant: String,
        /// The actual type found.
        actual_type: &'static str,
    },

    /// A tuple variant was given the wrong number of elements.
    TupleVariantWrongLength {
        /// The variant name.
        variant: String,
        /// Expected number of elements.
        expected: usize,
        /// Actual number of elements.
        actual: usize,
    },
}

impl fmt::Display for InvalidEnumKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyObject => {
                write!(f, "empty object cannot represent enum variant")
            }
            Self::MultipleKeys => {
                write!(f, "enum object must have exactly one key")
            }
            Self::VariantSelectionFailed => {
                write!(f, "failed to get selected variant")
            }
            Self::UnitVariantNotNull { variant, actual_type } => {
                write!(f, "unit variant '{variant}' expected null value, got {actual_type}")
            }
            Self::TupleVariantWrongLength {
                variant,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "tuple variant '{variant}' expected {expected} elements, got {actual}"
                )
            }
        }
    }
}

/// Deserialization errors.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum DeserializerError {
    /// Type mismatch between expected and actual value types.
    #[snafu(display("Type mismatch: expected {expected}, got {actual}"))]
    TypeMismatch {
        /// The expected type.
        expected: &'static str,
        /// The actual type found.
        actual: &'static str,
    },

    /// Numeric value is out of range for the target type.
    #[snafu(display("Value {value} is out of range for type {target_type} ({min}..={max})"))]
    NumericOverflow {
        /// The value that was out of range.
        value: i64,
        /// The target type name.
        target_type: &'static str,
        /// Minimum allowed value.
        min: i64,
        /// Maximum allowed value.
        max: i64,
    },

    /// Unsigned numeric value is out of range for the target type.
    #[snafu(display("Value {value} is out of range for type {target_type} (0..={max})"))]
    UnsignedNumericOverflow {
        /// The value that was out of range.
        value: u64,
        /// The target type name.
        target_type: &'static str,
        /// Maximum allowed value.
        max: u64,
    },

    /// The type is not supported for deserialization.
    #[snafu(display("Unsupported type: {type_name}"))]
    UnsupportedType {
        /// The unsupported type name.
        type_name: &'static str,
    },

    /// Invalid enum representation.
    #[snafu(display("Invalid enum: {reason}"))]
    InvalidEnum {
        /// The reason why the enum is invalid.
        reason: InvalidEnumKind,
    },

    /// Invalid value for the expected type.
    #[snafu(display("Invalid value: {message}"))]
    InvalidValue {
        /// Description of what was invalid.
        message: String,
    },

    /// Error from the reflection system.
    #[snafu(display("Reflection error: {source}"))]
    Reflection {
        /// The underlying reflection error.
        source: ReflectError,
    },

    /// Deserializer is in an invalid state.
    #[snafu(display("Deserializer in invalid state due to prior failure"))]
    InvalidState,
}

impl From<ReflectError> for DeserializerError {
    fn from(source: ReflectError) -> Self {
        DeserializerError::Reflection { source }
    }
}

impl From<InvalidEnumKind> for DeserializerError {
    fn from(reason: InvalidEnumKind) -> Self {
        DeserializerError::InvalidEnum { reason }
    }
}

macro_rules! try_narrow_int {
    (signed, $target:ty, $value:expr, $partial:expr) => {{
        let value = <$target>::try_from($value).map_err(|_| DeserializerError::NumericOverflow {
            value: $value,
            target_type: stringify!($target),
            min: <$target>::MIN as i64,
            max: <$target>::MAX as i64,
        })?;
        Ok($partial.set(value)?)
    }};
    (unsigned, $target:ty, $value:expr, $partial:expr) => {{
        let value = <$target>::try_from($value).map_err(|_| DeserializerError::UnsignedNumericOverflow {
            value: $value,
            target_type: stringify!($target),
            max: <$target>::MAX as u64,
        })?;
        Ok($partial.set(value)?)
    }};
}

pub struct Deserializer<'a, T>
where
    T: Facet<'a> + 'a,
{
    partial: Option<Partial<'a>>,
    _type: PhantomData<T>,
}

impl<'a, T> Deserializer<'a, T>
where
    T: Facet<'a> + 'a,
{
    /// Creates a new deserializer for the given type.
    pub fn new() -> Result<Self, DeserializerError> {
        let partial = Partial::alloc::<T>()?;

        Ok(Self {
            partial: Some(partial),
            _type: PhantomData,
        })
    }

    /// Applies a value to the deserializer.
    pub fn apply_value(&mut self, value: &'a Value) -> Result<(), DeserializerError> {
        let partial = self.partial.take().ok_or(DeserializerError::InvalidState)?;
        let partial = apply_value_to_partial(partial, value)?;
        self.partial = Some(partial);
        Ok(())
    }

    /// Builds the final deserialized value.
    pub fn build(mut self) -> Result<T, DeserializerError> {
        let partial = self.partial.take().ok_or(DeserializerError::InvalidState)?;
        let wip = partial.build()?;
        Ok(wip.materialize()?)
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_value_to_partial<'a>(partial: Partial<'a>, value: &'a Value) -> Result<Partial<'a>, DeserializerError> {
    match partial.shape().ty {
        Type::Primitive(pt) => apply_primitive_value_to_partial(pt, partial, value),
        Type::Sequence(st) => Err(err_unsupported_type("sequence")),
        Type::User(ut) => apply_user_value_to_partial(ut, partial, value),
        Type::Pointer(_) => Err(err_unsupported_type("pointer")),
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_primitive_value_to_partial<'a>(
    pt: PrimitiveType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    match pt {
        PrimitiveType::Boolean => Ok(partial.set(value_to_bool(value)?)?),
        PrimitiveType::Numeric(nt) => apply_numeric_value_to_partial(nt, partial, value),
        PrimitiveType::Textual(tt) => apply_textual_value_to_partial(tt, partial, value),
        PrimitiveType::Never => Err(err_unsupported_type("never")),
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_numeric_value_to_partial<'a>(
    nt: NumericType, partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, DeserializerError> {
    match nt {
        NumericType::Integer { signed: true } => {
            let int_value = value_to_i64(value)?;
            match partial.shape() {
                s if s == i8::SHAPE => try_narrow_int!(signed, i8, int_value, partial),
                s if s == i16::SHAPE => try_narrow_int!(signed, i16, int_value, partial),
                s if s == i32::SHAPE => try_narrow_int!(signed, i32, int_value, partial),
                s if s == i64::SHAPE => Ok(partial.set(int_value)?),
                _ => Err(err_unsupported_type("unknown signed integer")),
            }
        }
        NumericType::Integer { signed: false } => {
            let uint_value = value_to_u64(value)?;
            match partial.shape() {
                s if s == u8::SHAPE => try_narrow_int!(unsigned, u8, uint_value, partial),
                s if s == u16::SHAPE => try_narrow_int!(unsigned, u16, uint_value, partial),
                s if s == u32::SHAPE => try_narrow_int!(unsigned, u32, uint_value, partial),
                s if s == u64::SHAPE => Ok(partial.set(uint_value)?),
                _ => Err(err_unsupported_type("unknown unsigned integer")),
            }
        }
        NumericType::Float => match partial.shape() {
            s if s == f32::SHAPE => Ok(partial.set(value_to_f32(value)?)?),
            s if s == f64::SHAPE => Ok(partial.set(value_to_f64(value)?)?),
            _ => Err(err_unsupported_type("unknown float")),
        },
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_textual_value_to_partial<'a>(
    tt: TextualType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    let str_value = value_to_string(value)?;
    match tt {
        TextualType::Char => {
            let mut str_chars = str_value.chars();

            // We expect a single character, so if we got an empty string, or a string with two or more characters,
            // that's an error.
            let char_value = str_chars.next().ok_or_else(|| DeserializerError::InvalidValue {
                message: format!("expected a single character, got '{}'", str_value),
            })?;
            if str_chars.next().is_some() {
                return Err(DeserializerError::InvalidValue {
                    message: format!("expected a single character, got '{}'", str_value),
                });
            }

            Ok(partial.set(char_value)?)
        }
        TextualType::Str => Ok(partial.set(str_value.as_str())?),
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_user_value_to_partial<'a>(
    ut: UserType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    match ut {
        UserType::Struct(st) => apply_struct_value_to_partial(st, partial, value),
        UserType::Enum(_) => apply_enum_value_to_partial(partial, value),
        UserType::Union(_) => Err(err_unsupported_type("union")),
        UserType::Opaque => apply_opaque_value_to_partial(partial, value),
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_struct_value_to_partial<'a>(
    st: StructType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    match st.kind {
        StructKind::Unit => Ok(partial),
        StructKind::TupleStruct | StructKind::Tuple => apply_tuple_fields_to_partial(st.fields, partial, value),
        StructKind::Struct => apply_named_field_struct_value_to_partial(st.fields, partial, value),
    }
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_tuple_fields_to_partial<'a>(
    fields: &'static [Field], mut partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    let arr = value_to_array(value)?;

    if arr.len() != fields.len() {
        return Err(DeserializerError::InvalidValue {
            message: format!("expected {} elements for tuple, got {}", fields.len(), arr.len()),
        });
    }

    for (i, field_value) in arr.iter().enumerate() {
        trace!(index = i, "handling tuple field");
        partial = partial.begin_nth_field(i)?;
        partial = apply_value_to_partial(partial, field_value)?;
        partial = partial.end()?;
    }

    Ok(partial)
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_named_field_struct_value_to_partial<'a>(
    fields: &'static [Field], mut partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    let obj_value = value_to_object(value)?;

    for field in fields {
        trace!(field.name, "handling field");
        if let Some(field_value) = find_field_value(field, obj_value) {
            trace!(field.name, "found value for field, applying");
            partial = partial.begin_field(field.name)?;
            partial = apply_value_to_partial(partial, field_value)?;
            partial = partial.end()?;
        }
    }

    Ok(partial)
}

#[tracing::instrument(level = "trace", skip_all)]
fn apply_enum_value_to_partial<'a>(
    mut partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, DeserializerError> {
    match value.destructure_ref() {
        // Unit variant: "VariantName"
        DestructuredRef::String(variant_name) => {
            trace!(variant_name = variant_name.as_str(), "selecting unit variant");
            partial = partial.select_variant_named(variant_name)?;
            Ok(partial)
        }

        // Variant with data: {"VariantName": ...}
        DestructuredRef::Object(obj) => {
            // Get the single key (variant name)
            let mut iter = obj.iter();
            let (variant_name, variant_value) = iter.next().ok_or(InvalidEnumKind::EmptyObject)?;

            if iter.next().is_some() {
                return Err(InvalidEnumKind::MultipleKeys.into());
            }

            trace!(variant_name = variant_name.as_str(), "selecting variant");

            partial = partial.select_variant_named(variant_name)?;

            let variant = partial
                .selected_variant()
                .ok_or(InvalidEnumKind::VariantSelectionFailed)?;

            trace!(kind = ?variant.data.kind, "variant kind");

            // Deserialize based on variant kind
            match variant.data.kind {
                StructKind::Unit => {
                    // Unit variant in object form: {"Unit": null}
                    if !variant_value.is_null() {
                        return Err(InvalidEnumKind::UnitVariantNotNull {
                            variant: variant_name.to_string(),
                            actual_type: value_type(variant_value),
                        }
                        .into());
                    }
                }
                StructKind::TupleStruct | StructKind::Tuple => {
                    let fields = variant.data.fields;
                    if fields.is_empty() {
                        // Zero-field tuple, treat like unit
                        trace!("zero-field tuple variant");
                    } else if fields.len() == 1 {
                        // Newtype: {"X": value}
                        trace!("newtype variant");
                        partial = partial.begin_nth_field(0)?;
                        partial = apply_value_to_partial(partial, variant_value)?;
                        partial = partial.end()?;
                    } else {
                        // Tuple: {"Y": [v1, v2, ...]}
                        trace!(num_fields = fields.len(), "tuple variant");
                        let arr = value_to_array(variant_value)?;

                        if arr.len() != fields.len() {
                            return Err(InvalidEnumKind::TupleVariantWrongLength {
                                variant: variant_name.to_string(),
                                expected: fields.len(),
                                actual: arr.len(),
                            }
                            .into());
                        }

                        for (i, field_value) in arr.iter().enumerate() {
                            partial = partial.begin_nth_field(i)?;
                            partial = apply_value_to_partial(partial, field_value)?;
                            partial = partial.end()?;
                        }
                    }
                }
                StructKind::Struct => {
                    // Struct variant: {"Variant": {field1: v1, ...}}
                    trace!("struct variant");
                    partial = apply_named_field_struct_value_to_partial(variant.data.fields, partial, variant_value)?;
                }
            }

            Ok(partial)
        }

        _ => Err(err_type_mismatch("string or object", value)),
    }
}

#[tracing::instrument(level = "trace", skip_all, fields(shape = ?partial.shape()))]
fn apply_opaque_value_to_partial<'a>(partial: Partial<'a>, value: &'a Value) -> Result<Partial<'a>, DeserializerError> {
    // We destructure the value and basically just let `Partial::set` figure out if it can set
    // the value to the current field.
    //
    // TODO: Handle more specific cases -- known scalars, etc -- directly when `facet`/`facet-reflect` has support to do so.
    match value.destructure_ref() {
        DestructuredRef::Null => todo!(),
        DestructuredRef::Bool(bool_value) => {
            trace!("applying boolean value to opaque field");
            Ok(partial.set(bool_value)?)
        }
        DestructuredRef::Number(vnumber) => {
            if let Some(uint_value) = vnumber.to_u64() {
                trace!("applying unsigned integer value to opaque field");
                Ok(partial.set(uint_value)?)
            } else if let Some(int_value) = vnumber.to_i64() {
                trace!("applying signed integer value to opaque field");
                Ok(partial.set(int_value)?)
            } else if let Some(float_value) = vnumber.to_f64() {
                trace!("applying floating-point value to opaque field");
                Ok(partial.set(float_value)?)
            } else {
                Err(DeserializerError::InvalidValue {
                    message: "failed to convert number to concrete numeric type".to_string(),
                })
            }
        }
        DestructuredRef::String(str_value) => {
            trace!("applying string value to opaque field");
            Ok(partial.set(str_value.to_string())?)
        }
        DestructuredRef::Bytes(bytes_value) => {
            trace!("applying bytes value to opaque field");
            Ok(partial.set(bytes_value.to_vec())?)
        }
        DestructuredRef::Array(_) => todo!(),
        DestructuredRef::Object(_) => todo!(),
        DestructuredRef::DateTime(_) => todo!(),
        DestructuredRef::QName(_) => todo!(),
        DestructuredRef::Uuid(_) => todo!(),
    }
}

fn err_type_mismatch(expected: &'static str, value: &Value) -> DeserializerError {
    DeserializerError::TypeMismatch {
        expected,
        actual: value_type(value),
    }
}

fn err_unsupported_type(type_name: &'static str) -> DeserializerError {
    DeserializerError::UnsupportedType { type_name }
}

fn value_to_bool(value: &Value) -> Result<bool, DeserializerError> {
    value.as_bool().ok_or_else(|| err_type_mismatch("boolean", value))
}

fn value_to_i64(value: &Value) -> Result<i64, DeserializerError> {
    value
        .as_number()
        .and_then(|n| n.to_i64())
        .ok_or_else(|| err_type_mismatch("signed integer", value))
}

fn value_to_u64(value: &Value) -> Result<u64, DeserializerError> {
    value
        .as_number()
        .and_then(|n| n.to_u64())
        .ok_or_else(|| err_type_mismatch("unsigned integer", value))
}

fn value_to_f32(value: &Value) -> Result<f32, DeserializerError> {
    value
        .as_number()
        .map(|n| n.to_f64_lossy() as f32)
        .ok_or_else(|| err_type_mismatch("f32", value))
}

fn value_to_f64(value: &Value) -> Result<f64, DeserializerError> {
    value
        .as_number()
        .map(|n| n.to_f64_lossy())
        .ok_or_else(|| err_type_mismatch("f64", value))
}

fn value_to_string(value: &Value) -> Result<&facet_value::VString, DeserializerError> {
    value.as_string().ok_or_else(|| err_type_mismatch("string", value))
}

fn value_to_object(value: &Value) -> Result<&facet_value::VObject, DeserializerError> {
    value.as_object().ok_or_else(|| err_type_mismatch("object", value))
}

fn value_to_array(value: &Value) -> Result<&facet_value::VArray, DeserializerError> {
    value.as_array().ok_or_else(|| err_type_mismatch("array", value))
}

fn value_type(value: &Value) -> &'static str {
    match value.destructure_ref() {
        DestructuredRef::Null => "null",
        DestructuredRef::Bool(_) => "bool",
        DestructuredRef::Number(vnumber) => {
            if vnumber.is_float() {
                "float"
            } else if vnumber.to_u64().is_some() {
                "unsigned-int"
            } else {
                "signed-int"
            }
        }
        DestructuredRef::String(_) => "string",
        DestructuredRef::Bytes(_) => "bytes",
        DestructuredRef::Array(_) => "array",
        DestructuredRef::Object(_) => "object",
        DestructuredRef::DateTime(_) => "datetime",
        DestructuredRef::QName(_) => "qualified-name",
        DestructuredRef::Uuid(_) => "uuid",
    }
}

#[cfg(test)]
mod tests {
    use facet::Facet;
    use facet_value::value;

    use super::*;

    #[track_caller]
    fn build_partial<T, V>(input: V) -> T
    where
        for<'a> T: Facet<'a>,
        V: Into<Value>,
    {
        let value = input.into();

        let partial = Partial::alloc::<T>().unwrap();
        let partial = match apply_value_to_partial(partial, &value) {
            Ok(partial) => partial,
            Err(e) => panic!("Failed to apply value to partial: {:#}", e),
        };
        partial
            .build()
            .expect("Should not fail to build partial value.")
            .materialize()
            .expect("Should not fail to materialize partial value into concrete type.")
    }

    #[test]
    fn test_deserialize_basic() {
        #[derive(Facet)]
        struct Config {
            enabled: bool,
            api_key: String,
        }

        let input = value!({
            "enabled": true,
            "api_key": "123456",
        });

        let output: Config = build_partial(input);
        assert!(output.enabled);
        assert_eq!(output.api_key, "123456");
    }

    #[test]
    fn test_deserialize_basic_nested() {
        #[derive(Facet)]
        struct ProxyConfig {
            enabled: bool,
            http_proxy: String,
        }

        #[derive(Facet)]
        struct Config {
            enabled: bool,
            proxy: ProxyConfig,
        }

        let input = value!({
            "enabled": true,
            "proxy": {
                "enabled": true,
                "http_proxy": "http://example.com",
            },
        });

        let output: Config = build_partial(input);
        assert!(output.enabled);
        assert!(output.proxy.enabled);
        assert_eq!(output.proxy.http_proxy, "http://example.com");
    }

    #[test]
    fn test_deserialize_enum_unit_variant() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Status {
            Active,
            Inactive,
        }

        let input = value!("Active");
        let output: Status = build_partial(input);
        assert_eq!(output, Status::Active);

        let input = value!("Inactive");
        let output: Status = build_partial(input);
        assert_eq!(output, Status::Inactive);
    }

    #[test]
    fn test_deserialize_enum_newtype_variant() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Value {
            Int(i64),
            Text(String),
        }

        let input = value!({"Int": 42});
        let output: Value = build_partial(input);
        assert_eq!(output, Value::Int(42));

        let input = value!({"Text": "hello"});
        let output: Value = build_partial(input);
        assert_eq!(output, Value::Text("hello".to_string()));
    }

    #[test]
    fn test_deserialize_enum_tuple_variant() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Point {
            Coord(i32, i32),
            Coord3D(i32, i32, i32),
        }

        let input = value!({"Coord": [10, 20]});
        let output: Point = build_partial(input);
        assert_eq!(output, Point::Coord(10, 20));

        let input = value!({"Coord3D": [1, 2, 3]});
        let output: Point = build_partial(input);
        assert_eq!(output, Point::Coord3D(1, 2, 3));
    }

    #[test]
    fn test_deserialize_enum_struct_variant() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Message {
            Request { id: String, method: String },
            Response { id: String, result: i32 },
        }

        let input = value!({"Request": {"id": "1", "method": "ping"}});
        let output: Message = build_partial(input);
        assert_eq!(
            output,
            Message::Request {
                id: "1".to_string(),
                method: "ping".to_string()
            }
        );

        let input = value!({"Response": {"id": "1", "result": 42}});
        let output: Message = build_partial(input);
        assert_eq!(
            output,
            Message::Response {
                id: "1".to_string(),
                result: 42
            }
        );
    }

    #[test]
    fn test_deserialize_enum_in_struct() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Status {
            Active,
            Inactive,
        }

        #[derive(Facet, Debug, PartialEq)]
        struct Config {
            name: String,
            status: Status,
        }

        let input = value!({
            "name": "test",
            "status": "Active"
        });
        let output: Config = build_partial(input);
        assert_eq!(output.name, "test");
        assert_eq!(output.status, Status::Active);
    }

    #[test]
    fn test_deserialize_unit_struct() {
        #[derive(Facet, Debug, PartialEq)]
        struct Unit;

        let input = value!(null);
        let output: Unit = build_partial(input);
        assert_eq!(output, Unit);
    }

    #[test]
    fn test_deserialize_tuple_struct_single() {
        #[derive(Facet, Debug, PartialEq)]
        struct Wrapper(i32);

        let input = value!([42]);
        let output: Wrapper = build_partial(input);
        assert_eq!(output, Wrapper(42));
    }

    #[test]
    fn test_deserialize_tuple_struct_multi() {
        #[derive(Facet, Debug, PartialEq)]
        struct Point(i32, i32, i32);

        let input = value!([10, 20, 30]);
        let output: Point = build_partial(input);
        assert_eq!(output, Point(10, 20, 30));
    }

    #[test]
    fn test_deserialize_tuple() {
        let input = value!([42, "hello"]);
        let output: (i32, String) = build_partial(input);
        assert_eq!(output, (42, "hello".to_string()));
    }

    #[test]
    fn test_deserialize_nested_tuple_struct() {
        #[derive(Facet, Debug, PartialEq)]
        struct Inner(i32, i32);

        #[derive(Facet, Debug, PartialEq)]
        struct Outer {
            inner: Inner,
            flag: bool,
        }

        let input = value!({
            "inner": [1, 2],
            "flag": true
        });
        let output: Outer = build_partial(input);
        assert_eq!(
            output,
            Outer {
                inner: Inner(1, 2),
                flag: true
            }
        );
    }

    // ===================
    // Primitive type tests
    // ===================

    #[test]
    fn test_deserialize_bool() {
        let output: bool = build_partial(value!(true));
        assert!(output);

        let output: bool = build_partial(value!(false));
        assert!(!output);
    }

    #[test]
    fn test_deserialize_signed_integers() {
        let output: i8 = build_partial(value!(42));
        assert_eq!(output, 42i8);

        let output: i16 = build_partial(value!(1000));
        assert_eq!(output, 1000i16);

        let output: i32 = build_partial(value!(100_000));
        assert_eq!(output, 100_000i32);

        let output: i64 = build_partial(value!(10_000_000_000i64));
        assert_eq!(output, 10_000_000_000i64);

        // Negative values
        let output: i32 = build_partial(value!(-42));
        assert_eq!(output, -42i32);
    }

    #[test]
    fn test_deserialize_unsigned_integers() {
        let output: u8 = build_partial(value!(255));
        assert_eq!(output, 255u8);

        let output: u16 = build_partial(value!(65535));
        assert_eq!(output, 65535u16);

        let output: u32 = build_partial(value!(4_000_000_000u64));
        assert_eq!(output, 4_000_000_000u32);

        let output: u64 = build_partial(value!(10_000_000_000u64));
        assert_eq!(output, 10_000_000_000u64);
    }

    #[test]
    fn test_deserialize_floats() {
        #[derive(Facet, Debug)]
        struct F32Wrapper {
            v: f32,
        }

        #[derive(Facet, Debug)]
        struct F64Wrapper {
            v: f64,
        }

        let output: F32Wrapper = build_partial(value!({"v": 1.5}));
        assert!((output.v - 1.5f32).abs() < 0.001);

        let output: F64Wrapper = build_partial(value!({"v": 123.456789}));
        assert!((output.v - 123.456789f64).abs() < 0.0000001);

        // Integer to float
        let output: F64Wrapper = build_partial(value!({"v": 42}));
        assert_eq!(output.v, 42.0f64);
    }

    #[test]
    fn test_deserialize_char() {
        #[derive(Facet, Debug, PartialEq)]
        struct CharWrapper {
            c: char,
        }

        let output: CharWrapper = build_partial(value!({"c": "a"}));
        assert_eq!(output.c, 'a');

        let output: CharWrapper = build_partial(value!({"c": "Z"}));
        assert_eq!(output.c, 'Z');

        // Unicode
        let output: CharWrapper = build_partial(value!({"c": "🎉"}));
        assert_eq!(output.c, '🎉');
    }

    #[test]
    fn test_deserialize_string() {
        let output: String = build_partial(value!("hello world"));
        assert_eq!(output, "hello world");

        let output: String = build_partial(value!(""));
        assert_eq!(output, "");
    }

    // ===================
    // Error case tests
    // ===================

    #[track_caller]
    fn try_build_partial<T, V>(input: V) -> Result<T, DeserializerError>
    where
        for<'a> T: Facet<'a>,
        V: Into<Value>,
    {
        let value = input.into();
        let partial = Partial::alloc::<T>()?;
        let partial = apply_value_to_partial(partial, &value)?;
        let wip = partial.build()?;
        Ok(wip.materialize()?)
    }

    #[test]
    fn test_error_type_mismatch() {
        #[derive(Facet)]
        struct Config {
            count: i32,
        }

        // String where integer expected
        let result = try_build_partial::<Config, _>(value!({"count": "not a number"}));
        assert!(matches!(
            result,
            Err(DeserializerError::TypeMismatch {
                expected: "signed integer",
                ..
            })
        ));

        // Integer where string expected (String is opaque, so error comes from reflection layer)
        #[derive(Facet, Debug)]
        struct StringConfig {
            name: String,
        }
        let result = try_build_partial::<StringConfig, _>(value!({"name": 42}));
        assert!(matches!(result, Err(DeserializerError::Reflection { .. })));

        // Integer where bool expected
        #[derive(Facet)]
        struct BoolConfig {
            enabled: bool,
        }
        let result = try_build_partial::<BoolConfig, _>(value!({"enabled": 42}));
        assert!(matches!(
            result,
            Err(DeserializerError::TypeMismatch {
                expected: "boolean",
                ..
            })
        ));
    }

    #[test]
    fn test_error_numeric_overflow_signed() {
        #[derive(Facet)]
        struct I8Config {
            v: i8,
        }

        // Value too large for i8 (max 127)
        let result = try_build_partial::<I8Config, _>(value!({"v": 128}));
        assert!(matches!(
            result,
            Err(DeserializerError::NumericOverflow {
                value: 128,
                target_type: "i8",
                ..
            })
        ));

        // Value too small for i8 (min -128)
        let result = try_build_partial::<I8Config, _>(value!({"v": (-129)}));
        assert!(matches!(
            result,
            Err(DeserializerError::NumericOverflow {
                value: -129,
                target_type: "i8",
                ..
            })
        ));
    }

    #[test]
    fn test_error_numeric_overflow_unsigned() {
        #[derive(Facet)]
        struct U8Config {
            v: u8,
        }

        // Value too large for u8 (max 255)
        let result = try_build_partial::<U8Config, _>(value!({"v": 256}));
        assert!(matches!(
            result,
            Err(DeserializerError::UnsignedNumericOverflow {
                value: 256,
                target_type: "u8",
                ..
            })
        ));
    }

    #[test]
    fn test_error_invalid_char_empty() {
        #[derive(Facet)]
        struct CharConfig {
            c: char,
        }

        let result = try_build_partial::<CharConfig, _>(value!({"c": ""}));
        assert!(matches!(result, Err(DeserializerError::InvalidValue { .. })));
    }

    #[test]
    fn test_error_invalid_char_multi() {
        #[derive(Facet)]
        struct CharConfig {
            c: char,
        }

        let result = try_build_partial::<CharConfig, _>(value!({"c": "ab"}));
        assert!(matches!(result, Err(DeserializerError::InvalidValue { .. })));
    }

    #[test]
    fn test_error_enum_empty_object() {
        #[derive(Facet, Debug)]
        #[repr(u8)]
        enum Status {
            Active,
        }

        let result = try_build_partial::<Status, _>(value!({}));
        assert!(matches!(
            result,
            Err(DeserializerError::InvalidEnum {
                reason: InvalidEnumKind::EmptyObject
            })
        ));
    }

    #[test]
    fn test_error_enum_multiple_keys() {
        #[derive(Facet, Debug)]
        #[repr(u8)]
        enum Status {
            Active,
            Inactive,
        }

        let result = try_build_partial::<Status, _>(value!({"Active": null, "Inactive": null}));
        assert!(matches!(
            result,
            Err(DeserializerError::InvalidEnum {
                reason: InvalidEnumKind::MultipleKeys
            })
        ));
    }

    #[test]
    fn test_error_enum_unit_not_null() {
        #[derive(Facet, Debug)]
        #[repr(u8)]
        enum Status {
            Active,
        }

        let result = try_build_partial::<Status, _>(value!({"Active": 42}));
        assert!(matches!(
            result,
            Err(DeserializerError::InvalidEnum {
                reason: InvalidEnumKind::UnitVariantNotNull { .. }
            })
        ));
    }

    #[test]
    fn test_error_enum_tuple_wrong_length() {
        #[derive(Facet, Debug)]
        #[repr(u8)]
        enum Point {
            Coord(i32, i32),
        }

        // Too few elements
        let result = try_build_partial::<Point, _>(value!({"Coord": [1]}));
        assert!(matches!(
            result,
            Err(DeserializerError::InvalidEnum {
                reason: InvalidEnumKind::TupleVariantWrongLength {
                    expected: 2,
                    actual: 1,
                    ..
                }
            })
        ));

        // Too many elements
        let result = try_build_partial::<Point, _>(value!({"Coord": [1, 2, 3]}));
        assert!(matches!(
            result,
            Err(DeserializerError::InvalidEnum {
                reason: InvalidEnumKind::TupleVariantWrongLength {
                    expected: 2,
                    actual: 3,
                    ..
                }
            })
        ));
    }

    #[test]
    fn test_error_tuple_wrong_length() {
        #[derive(Facet, Debug)]
        struct Point(i32, i32);

        // Too few elements
        let result = try_build_partial::<Point, _>(value!([1]));
        assert!(matches!(result, Err(DeserializerError::InvalidValue { .. })));

        // Too many elements
        let result = try_build_partial::<Point, _>(value!([1, 2, 3]));
        assert!(matches!(result, Err(DeserializerError::InvalidValue { .. })));
    }

    // ===================
    // Edge case tests
    // ===================

    #[test]
    fn test_deserialize_boundary_values() {
        #[derive(Facet, Debug, PartialEq)]
        struct Boundaries {
            i8_min: i8,
            i8_max: i8,
            u8_max: u8,
            i16_min: i16,
            i16_max: i16,
            u16_max: u16,
            i32_min: i32,
            i32_max: i32,
            u32_max: u32,
        }

        let output: Boundaries = build_partial(value!({
            "i8_min": (-128),
            "i8_max": 127,
            "u8_max": 255,
            "i16_min": (-32768),
            "i16_max": 32767,
            "u16_max": 65535,
            "i32_min": (-2147483648i64),
            "i32_max": 2147483647,
            "u32_max": 4294967295u64
        }));

        assert_eq!(output.i8_min, i8::MIN);
        assert_eq!(output.i8_max, i8::MAX);
        assert_eq!(output.u8_max, u8::MAX);
        assert_eq!(output.i16_min, i16::MIN);
        assert_eq!(output.i16_max, i16::MAX);
        assert_eq!(output.u16_max, u16::MAX);
        assert_eq!(output.i32_min, i32::MIN);
        assert_eq!(output.i32_max, i32::MAX);
        assert_eq!(output.u32_max, u32::MAX);
    }

    #[test]
    fn test_deserialize_extra_fields_ignored() {
        #[derive(Facet, Debug, PartialEq)]
        struct Config {
            name: String,
            count: i32,
        }

        // Extra fields "extra" and "unknown" should be ignored
        let output: Config = build_partial(value!({
            "name": "test",
            "count": 42,
            "extra": "ignored",
            "unknown": true
        }));

        assert_eq!(output.name, "test");
        assert_eq!(output.count, 42);
    }

    #[test]
    fn test_deserialize_deeply_nested() {
        #[derive(Facet, Debug, PartialEq)]
        struct Level3 {
            value: i32,
        }

        #[derive(Facet, Debug, PartialEq)]
        struct Level2 {
            level3: Level3,
        }

        #[derive(Facet, Debug, PartialEq)]
        struct Level1 {
            level2: Level2,
        }

        #[derive(Facet, Debug, PartialEq)]
        struct Root {
            level1: Level1,
        }

        let output: Root = build_partial(value!({
            "level1": {
                "level2": {
                    "level3": {
                        "value": 42
                    }
                }
            }
        }));

        assert_eq!(output.level1.level2.level3.value, 42);
    }

    #[test]
    fn test_deserialize_empty_tuple() {
        let _output: () = build_partial(value!([]));
        // If we get here without panic, the test passes
    }

    #[test]
    fn test_deserialize_mixed_enum() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Mixed {
            Unit,
            Newtype(i32),
            Tuple(i32, String),
            Struct { x: i32, y: i32 },
        }

        let output: Mixed = build_partial(value!("Unit"));
        assert_eq!(output, Mixed::Unit);

        let output: Mixed = build_partial(value!({"Newtype": 42}));
        assert_eq!(output, Mixed::Newtype(42));

        let output: Mixed = build_partial(value!({"Tuple": [1, "hello"]}));
        assert_eq!(output, Mixed::Tuple(1, "hello".to_string()));

        let output: Mixed = build_partial(value!({"Struct": {"x": 10, "y": 20}}));
        assert_eq!(output, Mixed::Struct { x: 10, y: 20 });
    }

    #[test]
    fn test_deserialize_renamed_field() {
        #[derive(Facet, Debug, PartialEq)]
        struct Config {
            #[facet(rename = "api_key")]
            key: String,
        }

        let input = value!({"api_key": "secret123"});
        let output: Config = build_partial(input);
        assert_eq!(output.key, "secret123");
    }

    #[test]
    fn test_deserialize_multiple_renamed_fields() {
        #[derive(Facet, Debug, PartialEq)]
        struct Config {
            #[facet(rename = "firstName")]
            first_name: String,
            #[facet(rename = "lastName")]
            last_name: String,
            age: u32, // No rename
        }

        let input = value!({
            "firstName": "John",
            "lastName": "Doe",
            "age": 30
        });
        let output: Config = build_partial(input);
        assert_eq!(output.first_name, "John");
        assert_eq!(output.last_name, "Doe");
        assert_eq!(output.age, 30);
    }

    #[test]
    fn test_deserialize_nested_renamed_fields() {
        #[derive(Facet, Debug, PartialEq)]
        struct Address {
            #[facet(rename = "streetAddress")]
            street: String,
        }

        #[derive(Facet, Debug, PartialEq)]
        struct Person {
            name: String,
            #[facet(rename = "homeAddress")]
            address: Address,
        }

        let input = value!({
            "name": "Alice",
            "homeAddress": {
                "streetAddress": "123 Main St"
            }
        });
        let output: Person = build_partial(input);
        assert_eq!(output.name, "Alice");
        assert_eq!(output.address.street, "123 Main St");
    }

    #[test]
    fn test_deserialize_enum_struct_variant_with_rename() {
        #[derive(Facet, Debug, PartialEq)]
        #[repr(u8)]
        enum Message {
            Data {
                #[facet(rename = "messageId")]
                id: String,
                content: String,
            },
        }

        let input = value!({"Data": {"messageId": "msg-001", "content": "hello"}});
        let output: Message = build_partial(input);
        match output {
            Message::Data { id, content } => {
                assert_eq!(id, "msg-001");
                assert_eq!(content, "hello");
            }
        }
    }

    // ===================
    // Alias tests
    // ===================
    // NOTE: The following alias tests are disabled because the facet derive macro
    // does not yet recognize the `alias` attribute. Once the macro is updated to accept
    // `alias` as a valid attribute, enable these tests by defining the cfg flag.
    // To enable: add `--cfg test_with_alias` to RUSTFLAGS when running tests.

    #[cfg(test_with_alias)]
    mod alias_tests {
        use super::*;

        #[test]
        fn test_deserialize_field_with_single_alias() {
            #[derive(Facet, Debug, PartialEq)]
            struct Config {
                #[facet(alias = "key")]
                api_key: String,
            }

            // Try with original name
            let input = value!({"api_key": "secret"});
            let output: Config = build_partial(input);
            assert_eq!(output.api_key, "secret");

            // Try with alias
            let input = value!({"key": "secret2"});
            let output: Config = build_partial(input);
            assert_eq!(output.api_key, "secret2");
        }

        #[test]
        fn test_deserialize_field_with_multiple_aliases() {
            #[derive(Facet, Debug, PartialEq)]
            struct Config {
                #[facet(alias = "key")]
                #[facet(alias = "apiKey")]
                #[facet(alias = "api-key")]
                api_key: String,
            }

            // Original name has priority
            let input = value!({"api_key": "primary", "key": "fallback"});
            let output: Config = build_partial(input);
            assert_eq!(output.api_key, "primary");

            // First alias
            let input = value!({"key": "first"});
            let output: Config = build_partial(input);
            assert_eq!(output.api_key, "first");

            // Second alias
            let input = value!({"apiKey": "second"});
            let output: Config = build_partial(input);
            assert_eq!(output.api_key, "second");

            // Third alias
            let input = value!({"api-key": "third"});
            let output: Config = build_partial(input);
            assert_eq!(output.api_key, "third");
        }

        #[test]
        fn test_deserialize_rename_with_aliases() {
            #[derive(Facet, Debug, PartialEq)]
            struct Config {
                #[facet(rename = "newName")]
                #[facet(alias = "oldName")]
                #[facet(alias = "legacyName")]
                field: String,
            }

            // Renamed name has highest priority
            let input = value!({"newName": "new", "field": "orig", "oldName": "old"});
            let output: Config = build_partial(input);
            assert_eq!(output.field, "new");

            // Original field name is second priority
            let input = value!({"field": "orig", "oldName": "old"});
            let output: Config = build_partial(input);
            assert_eq!(output.field, "orig");

            // First alias is third priority
            let input = value!({"oldName": "old", "legacyName": "legacy"});
            let output: Config = build_partial(input);
            assert_eq!(output.field, "old");

            // Last alias
            let input = value!({"legacyName": "legacy"});
            let output: Config = build_partial(input);
            assert_eq!(output.field, "legacy");
        }

        #[test]
        fn test_deserialize_nested_struct_with_aliases() {
            #[derive(Facet, Debug, PartialEq)]
            struct Inner {
                #[facet(alias = "val")]
                value: i32,
            }

            #[derive(Facet, Debug, PartialEq)]
            struct Outer {
                #[facet(alias = "data")]
                inner: Inner,
            }

            let input = value!({"data": {"val": 42}});
            let output: Outer = build_partial(input);
            assert_eq!(output.inner.value, 42);
        }

        #[test]
        fn test_deserialize_enum_variant_with_alias() {
            #[derive(Facet, Debug, PartialEq)]
            #[repr(u8)]
            enum Message {
                Data {
                    #[facet(alias = "id")]
                    message_id: String,
                },
            }

            // Original name
            let input = value!({"Data": {"message_id": "msg-1"}});
            let output: Message = build_partial(input);
            match output {
                Message::Data { message_id } => assert_eq!(message_id, "msg-1"),
            }

            // Alias
            let input = value!({"Data": {"id": "msg-2"}});
            let output: Message = build_partial(input);
            match output {
                Message::Data { message_id } => assert_eq!(message_id, "msg-2"),
            }
        }

        #[test]
        fn test_alias_priority_order() {
            #[derive(Facet, Debug, PartialEq)]
            struct Config {
                #[facet(alias = "second")]
                #[facet(alias = "third")]
                first: String,
            }

            // When multiple names present, original field name wins
            let input = value!({"first": "1", "second": "2", "third": "3"});
            let output: Config = build_partial(input);
            assert_eq!(output.first, "1");

            // When original missing, first alias wins
            let input = value!({"second": "2", "third": "3"});
            let output: Config = build_partial(input);
            assert_eq!(output.first, "2");
        }
    }

    // ===================
    // Deserializer API tests
    // ===================

    #[test]
    fn test_deserializer_api_success() {
        #[derive(Facet, Debug, PartialEq)]
        struct Config {
            name: String,
            value: i32,
        }

        let value = value!({
            "name": "test",
            "value": 42
        });

        let mut deser = Deserializer::<Config>::new().expect("should create deserializer");
        deser.apply_value(&value).expect("should apply value");
        let result = deser.build().expect("should build");

        assert_eq!(result.name, "test");
        assert_eq!(result.value, 42);
    }
}
