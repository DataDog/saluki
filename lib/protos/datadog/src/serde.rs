use std::{fmt, marker::PhantomData};

use protobuf::{Enum, EnumOrUnknown};
use serde::{Deserializer, Serializer};

pub fn serialize_proto_enum<E: Enum, S: Serializer>(e: &EnumOrUnknown<E>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_i32(e.value())
}

pub fn deserialize_proto_enum<'de, E: Enum, D: Deserializer<'de>>(d: D) -> Result<EnumOrUnknown<E>, D::Error> {
    struct DeserializeEnumVisitor<E: Enum>(PhantomData<E>);

    impl<'de, E: Enum> serde::de::Visitor<'de> for DeserializeEnumVisitor<E> {
        type Value = EnumOrUnknown<E>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "an integer")
        }

        fn visit_i32<R>(self, v: i32) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v))
        }
    }

    d.deserialize_any(DeserializeEnumVisitor(PhantomData))
}
