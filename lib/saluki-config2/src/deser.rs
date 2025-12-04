use std::{marker::PhantomData, u8};

use facet::{
    EnumType, Facet, Field, NumericType, PointerType, PrimitiveType, SequenceType, Shape, ShapeLayout, StructType,
    TextualType, Type, UserType,
};
use facet_reflect::Partial;
use facet_value::{DestructuredRef, Value};
use saluki_error::{generic_error, ErrorContext, GenericError};

macro_rules! oversized_numeric {
    ($ty:ty, $value:expr) => {
        generic_error!(
            "Value {} is too large for type '{}' ({}-{}).",
            $value,
            stringify!($ty),
            <$ty>::MIN,
            <$ty>::MAX
        )
    };
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
    pub fn new() -> Result<Self, GenericError> {
        let partial = Partial::alloc::<T>().error_context("Failed to allocate storage for deserializing object.")?;

        Ok(Self {
            partial: Some(partial),
            _type: PhantomData,
        })
    }

    pub fn apply_value(&mut self, value: &'a Value) -> Result<(), GenericError> {
        match self.partial.take() {
            Some(partial) => {
                let new_partial = apply_value_to_partial(partial, value)?;
                self.partial = Some(new_partial);
                Ok(())
            }
            None => Err(generic_error!(
                "Deserializer in non-deterministic state due to prior failure.",
            )),
        }
    }

    pub fn build(mut self) -> Result<T, GenericError> {
        match self.partial.take() {
            Some(partial) => partial
                .build()
                .error_context("Failed to build object from partial.")?
                .materialize()
                .error_context("Failed to materialize object to specified type."),
            None => Err(generic_error!(
                "Deserializer in non-deterministic state due to prior failure.",
            )),
        }
    }
}

fn apply_value_to_partial<'a>(partial: Partial<'a>, value: &'a Value) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_value_to_partial");

    match partial.shape().ty {
        Type::Primitive(pt) => apply_primitive_value_to_partial(pt, partial, value),
        Type::Sequence(st) => apply_sequence_value_to_partial(st, partial, value),
        Type::User(ut) => apply_user_value_to_partial(ut, partial, value),
        Type::Pointer(pt) => apply_pointer_value_to_partial(pt, partial, value),
    }
}

fn apply_primitive_value_to_partial<'a>(
    pt: PrimitiveType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_primitive_value_to_partial");

    match pt {
        PrimitiveType::Boolean => value
            .as_bool()
            .ok_or_else(|| type_mismatch("boolean", value))
            .and_then(|value| set_partial_value(partial, value)),
        PrimitiveType::Numeric(nt) => apply_numeric_value_to_partial(nt, partial, value),
        PrimitiveType::Textual(tt) => apply_textual_value_to_partial(tt, partial, value),
        // TODO: Add frame path to error.
        PrimitiveType::Never => Err(generic_error!("Never type cannot be deserialized.")),
    }
}

fn apply_numeric_value_to_partial<'a>(
    nt: NumericType, partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_numeric_value_to_partial");

    match nt {
        NumericType::Integer { signed } => {
            if signed {
                assign_signed_integer_value_to_partial(partial, value)
            } else {
                assign_unsigned_integer_value_to_partial(partial, value)
            }
        }
        NumericType::Float => assign_float_value_to_partial(partial, value),
    }
}

fn assign_signed_integer_value_to_partial<'a>(
    partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_signed_integer_value_to_partial");

    let int_value = value
        .as_number()
        .and_then(|n| n.to_i64())
        .ok_or_else(|| type_mismatch("int", value))?;

    match get_shape_bitwidth(partial.shape()) {
        i8::BITS => {
            let value = i8::try_from(int_value).map_err(|_| oversized_numeric!(i8, int_value))?;
            set_partial_value(partial, value)
        }
        i16::BITS => {
            let value = i16::try_from(int_value).map_err(|_| oversized_numeric!(i16, int_value))?;
            set_partial_value(partial, value)
        }
        i32::BITS => {
            let value = i32::try_from(int_value).map_err(|_| oversized_numeric!(i32, int_value))?;
            set_partial_value(partial, value)
        }
        i64::BITS => set_partial_value(partial, int_value),
        _ => Err(generic_error!("Unhandled signed integer type '{}'.", partial.shape())),
    }
}

fn assign_unsigned_integer_value_to_partial<'a>(
    partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_unsigned_integer_value_to_partial");

    let uint_value = value
        .as_number()
        .and_then(|n| n.to_u64())
        .ok_or_else(|| type_mismatch("uint", value))?;

    match get_shape_bitwidth(partial.shape()) {
        u8::BITS => {
            let value = u8::try_from(uint_value).map_err(|_| oversized_numeric!(u8, uint_value))?;
            set_partial_value(partial, value)
        }
        u16::BITS => {
            let value = u16::try_from(uint_value).map_err(|_| oversized_numeric!(u16, uint_value))?;
            set_partial_value(partial, value)
        }
        u32::BITS => {
            let value = u32::try_from(uint_value).map_err(|_| oversized_numeric!(u32, uint_value))?;
            set_partial_value(partial, value)
        }
        u64::BITS => set_partial_value(partial, uint_value),
        _ => Err(generic_error!("Unhandled unsigned integer type '{}'.", partial.shape())),
    }
}

fn assign_float_value_to_partial<'a>(partial: Partial<'a>, value: &Value) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_float_value_to_partial");

    if partial.shape() == f32::SHAPE {
        value
            .as_number()
            .and_then(|n| n.to_f32())
            .ok_or_else(|| type_mismatch("f32", value))
            .and_then(|value| set_partial_value(partial, value))
    } else if partial.shape() == f64::SHAPE {
        value
            .as_number()
            .and_then(|n| n.to_f64())
            .ok_or_else(|| type_mismatch("f64", value))
            .and_then(|value| set_partial_value(partial, value))
    } else {
        Err(generic_error!("Unhandled float type '{}'.", partial.shape()))
    }
}

fn apply_textual_value_to_partial<'a>(
    tt: TextualType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_textual_value_to_partial");

    let str_value = value.as_string().ok_or_else(|| type_mismatch("string", value))?;
    match tt {
        TextualType::Char => {
            let mut str_chars = str_value.chars();

            // We expect a single character, so if we got an empty string, or a string with two or more characters, that's an error.
            let char_value = str_chars
                .next()
                .ok_or_else(|| generic_error!("Expected a single character, got '{}'.", str_value))?;
            if str_chars.next().is_some() {
                return Err(generic_error!("Expected a single character, got '{}'.", str_value));
            }

            set_partial_value(partial, char_value)
        }
        TextualType::Str => set_partial_value(partial, str_value.as_str()),
    }
}

fn apply_sequence_value_to_partial<'a>(
    st: SequenceType, partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_sequence_value_to_partial");

    Err(generic_error!("Sequences are not supported."))
}

fn apply_user_value_to_partial<'a>(
    ut: UserType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_user_value_to_partial");

    match ut {
        UserType::Struct(st) => apply_struct_value_to_partial(st, partial, value),
        UserType::Enum(et) => apply_enum_value_to_partial(et, partial, value),
        UserType::Union(_) => Err(generic_error!("Union types are not supported.")),
        UserType::Opaque => apply_opaque_value_to_partial(partial, value),
    }
}

fn apply_struct_value_to_partial<'a>(
    st: StructType, partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_struct_value_to_partial");

    match st.kind {
        facet::StructKind::Unit => todo!(),
        facet::StructKind::TupleStruct => todo!(),
        facet::StructKind::Struct => apply_named_field_struct_value_to_partial(st.fields, partial, value),
        facet::StructKind::Tuple => todo!(),
    }
}

fn apply_named_field_struct_value_to_partial<'a>(
    fields: &'static [Field], mut partial: Partial<'a>, value: &'a Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_named_field_struct_value_to_partial");

    let obj_value = value.as_object().ok_or_else(|| type_mismatch("object", value))?;

    for field in fields {
        debug_log(&partial, format!("handling field {}", field.name));
        if let Some(field_value) = obj_value.get(field.name) {
            debug_log(&partial, format!("found value for field {}, applying...", field.name));
            partial = partial.begin_field(field.name)?;
            partial = apply_value_to_partial(partial, field_value)?;
            partial = partial.end()?;
        }
    }

    Ok(partial)
}

fn apply_enum_value_to_partial<'a>(
    et: EnumType, partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_enum_value_to_partial");

    todo!()
}

fn apply_opaque_value_to_partial<'a>(partial: Partial<'a>, value: &'a Value) -> Result<Partial<'a>, GenericError> {
    debug_log(
        &partial,
        format!("apply_opaque_value_to_partial. shape: {:?}", partial.shape()),
    );

    // We destructure the value and basically just let `Partial::set` figure out if it can set
    // the value to the current field.
    //
    // TODO: Handle more specific cases -- known scalars, etc -- directly when `facet`/`facet-reflect` has support to do so.
    match value.destructure_ref() {
        DestructuredRef::Null => todo!(),
        DestructuredRef::Bool(bool_value) => {
            debug_log(&partial, "applying boolean value to opaque field");
            set_partial_value(partial, bool_value)
        }
        DestructuredRef::Number(vnumber) => {
            if let Some(uint_value) = vnumber.to_u64() {
                debug_log(&partial, "applying unsigned integer value to opaque field");
                set_partial_value(partial, uint_value)
            } else if let Some(int_value) = vnumber.to_i64() {
                debug_log(&partial, "applying signed integer value to opaque field");
                set_partial_value(partial, int_value)
            } else if let Some(float_value) = vnumber.to_f64() {
                debug_log(&partial, "applying floating-point value to opaque field");
                set_partial_value(partial, float_value)
            } else {
                Err(generic_error!("Failed to convert number to concrete numeric type."))
            }
        }
        DestructuredRef::String(str_value) => {
            debug_log(&partial, "applying string value to opaque field");
            set_partial_value(partial, str_value.to_string())
        }
        DestructuredRef::Bytes(bytes_value) => {
            debug_log(&partial, "applying bytes value to opaque field");
            set_partial_value(partial, bytes_value.to_vec())
        }
        DestructuredRef::Array(_) => todo!(),
        DestructuredRef::Object(_) => todo!(),
        DestructuredRef::DateTime(_) => todo!(),
    }
}

fn apply_pointer_value_to_partial<'a>(
    pt: PointerType, partial: Partial<'a>, value: &Value,
) -> Result<Partial<'a>, GenericError> {
    debug_log(&partial, "apply_pointer_value_to_partial");

    todo!()
}

fn get_shape_bitwidth(shape: &Shape) -> u32 {
    match shape.layout {
        ShapeLayout::Sized(layout) => u32::try_from(layout.size() * 8).unwrap_or(u32::MAX),
        ShapeLayout::Unsized => 0,
    }
}

fn set_partial_value<'a, V: Facet<'a>>(partial: Partial<'a>, value: V) -> Result<Partial<'a>, GenericError> {
    let path = partial.path();
    partial
        .set(value)
        .with_error_context(|| format!("Failed to set value for {}.", path))
}

fn type_mismatch(expected: &'static str, value: &Value) -> GenericError {
    generic_error!("Type mismatch: expected {}, got {}", expected, value_type(value))
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
    }
}

fn debug_log<'a, S>(_partial: &Partial<'a>, _message: S)
where
    S: AsRef<str>,
{
    #[cfg(test)]
    {
        let indent = _partial.frame_count().saturating_sub(1);
        let indent_str = "  ".repeat(indent);

        println!("{}{}", indent_str, _message.as_ref());
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
        assert_eq!(output.enabled, true);
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
        assert_eq!(output.enabled, true);
        assert_eq!(output.proxy.enabled, true);
        assert_eq!(output.proxy.http_proxy, "http://example.com");
    }
}
