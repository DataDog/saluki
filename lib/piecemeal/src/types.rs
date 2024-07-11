use crate::{helpers::WireType, ProtoResult, Writer};

pub mod protobuf {
    macro_rules! generate_protobuf_primitive_types {
		(proto_ty => $ty_name:ident, wire_type => $wire_type:ident, rust_types => [$($rust_ty:ty),+], write => { func => $write_fn:ident, deref, as_type => $as_ty:ty } $(,)?) => {
			$(
				impl $crate::types::ProtobufValue<$rust_ty> for $ty_name {
					fn wire_type() -> $crate::helpers::WireType {
						$crate::helpers::WireType::$wire_type
					}

					fn packable() -> bool {
						true
					}

					fn write_value<W: $crate::Writer>(writer: &mut W, value: &$rust_ty) -> $crate::ProtoResult<()> {
						writer.$write_fn(*value as $as_ty)
					}
				}
			)+
		};
	}

    macro_rules! generate_protobuf_ref_types {
		(proto_ty => $ty_name:ident, wire_type => $wire_type:ident, rust_types => [$($rust_ty:ty),+], write => { func => $write_fn:ident } $(,)?) => {
			$(
				impl $crate::types::ProtobufValue<$rust_ty> for $ty_name {
					fn wire_type() -> $crate::helpers::WireType {
						$crate::helpers::WireType::$wire_type
					}

					fn write_value<W: $crate::Writer>(writer: &mut W, value: &$rust_ty) -> $crate::ProtoResult<()> {
						writer.$write_fn(value)
					}
				}
			)+
		};
	}

    pub struct Varint;
    pub struct Sint32;
    pub struct Sint64;
    pub struct Fixed32;
    pub struct Fixed64;
    pub struct Sfixed32;
    pub struct Sfixed64;
    pub struct Bytes;

    generate_protobuf_primitive_types!(
        proto_ty => Varint,
        wire_type => Varint,
        rust_types => [i8, i16, i32, i64, isize, u8, u16, u32, u64, usize],
        write => { func => write_varint, deref, as_type => u64 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Sint32,
        wire_type => Varint,
        rust_types => [i8, i16, i32],
        write => { func => write_sint32, deref, as_type => i32 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Sint64,
        wire_type => Varint,
        rust_types => [i8, i16, i32, i64, isize],
        write => { func => write_sint64, deref, as_type => i64 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Fixed32,
        wire_type => Fixed32,
        rust_types => [u8, u16, u32],
        write => { func => write_fixed32, deref, as_type => u32 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Fixed64,
        wire_type => Fixed64,
        rust_types => [u8, u16, u32, u64, usize],
        write => { func => write_fixed64, deref, as_type => u64 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Sfixed32,
        wire_type => Fixed32,
        rust_types => [i8, i16, i32],
        write => { func => write_sfixed32, deref, as_type => i32 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Sfixed32,
        wire_type => Fixed32,
        rust_types => [f32],
        write => { func => write_float, deref, as_type => f32 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Sfixed64,
        wire_type => Fixed64,
        rust_types => [i8, i16, i32, i64, isize],
        write => { func => write_sfixed64, deref, as_type => i64 },
    );
    generate_protobuf_primitive_types!(
        proto_ty => Sfixed64,
        wire_type => Fixed64,
        rust_types => [f64],
        write => { func => write_double, deref, as_type => f64 },
    );
    generate_protobuf_ref_types!(
        proto_ty => Bytes,
        wire_type => LengthDelimited,
        rust_types => [str],
        write => { func => write_string },
    );
}

/// A non-complex value type, excluding strings and bytes.
///
/// This is effectively all numeric types and booleans.
///
/// Another way to view primitivs types is that they all have a bounded size and can potentially be
/// encoded contiguously without the need for a field tag between each value. This property is what
/// allows for packed repeated fields when the field is a primitive type.
pub trait Primitive {}

impl<T> Primitive for T where T: ProtobufValue<T> {}

/// A non-complex value type.
///
/// Scalar values include primitive values as well as strings and bytes.
///
/// Essentially, any value that isn't an object or map is a scalar type.
pub trait Scalar {}

impl<T> Scalar for T where T: ProtobufValue<T> {}

pub trait ProtobufValue<T: ?Sized> {
    fn wire_type() -> WireType;

    fn packable() -> bool {
        false
    }

    fn write_value<W: Writer>(writer: &mut W, value: &T) -> ProtoResult<()>;
}

macro_rules! impl_basic_traits {
	(primitive => [$($t:ty),+]) => {
		$(
			impl Primitive for $t {}
		)+
	};
	(scalar => [$($t:ty),+]) => {
		$(
			impl Scalar for $t {}
		)+
	};
}

impl_basic_traits!(primitive => [bool, u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64]);
impl_basic_traits!(scalar => [bool, u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64, str, [u8]]);
