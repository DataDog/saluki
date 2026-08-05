use std::{collections::HashMap, fmt, hash::Hash, marker::PhantomData};

use protobuf::{Enum, EnumOrUnknown, MessageField};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

        fn visit_i8<R>(self, v: i8) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v as i32))
        }

        fn visit_i16<R>(self, v: i16) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v as i32))
        }

        fn visit_i32<R>(self, v: i32) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v))
        }

        fn visit_u8<R>(self, v: u8) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v as i32))
        }

        fn visit_u16<R>(self, v: u16) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v as i32))
        }
    }

    d.deserialize_any(DeserializeEnumVisitor(PhantomData))
}

pub fn serialize_proto_message<T: Serialize, S: Serializer>(
    message: &MessageField<T>, s: S,
) -> Result<S::Ok, S::Error> {
    match message.as_ref() {
        Some(inner) => s.serialize_some(inner),
        None => s.serialize_none(),
    }
}

pub fn deserialize_proto_message<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    d: D,
) -> Result<MessageField<T>, D::Error> {
    Option::<T>::deserialize(d).map(MessageField::from)
}

pub fn serialize_proto_repeated<T: Serialize, S: Serializer>(items: &Vec<T>, s: S) -> Result<S::Ok, S::Error> {
    items.serialize(s)
}

pub fn deserialize_proto_repeated<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    Vec::<T>::deserialize(d)
}

pub fn serialize_proto_map<K: Serialize, V: Serialize, S: Serializer>(
    items: &HashMap<K, V>, s: S,
) -> Result<S::Ok, S::Error> {
    items.serialize(s)
}

pub fn deserialize_proto_map<'de, K: Deserialize<'de> + Eq + Hash, V: Deserialize<'de>, D: Deserializer<'de>>(
    d: D,
) -> Result<HashMap<K, V>, D::Error> {
    HashMap::<K, V>::deserialize(d)
}

pub fn serialize_proto_bytes<S: Serializer>(bytes: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
    serde_bytes::serialize(bytes.as_slice(), s)
}

pub fn deserialize_proto_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    serde_bytes::ByteBuf::deserialize(d).map(|bb| bb.into_vec())
}

#[cfg(test)]
mod tests {
    use serde::de::IntoDeserializer as _;

    use super::*;
    use crate::metrics::MetricType;

    #[derive(Serialize)]
    struct EnumHolder {
        #[serde(serialize_with = "serialize_proto_enum")]
        value: EnumOrUnknown<MetricType>,
    }

    #[derive(Serialize, Deserialize)]
    struct BytesHolder {
        #[serde(
            serialize_with = "serialize_proto_bytes",
            deserialize_with = "deserialize_proto_bytes",
            default
        )]
        data: Vec<u8>,
    }

    #[test]
    fn serialize_proto_enum_emits_the_wire_integer_for_known_and_unknown_values() {
        // `MetricType::RATE` has wire value 2; an unknown discriminant is emitted verbatim.
        let known = serde_json::to_value(EnumHolder {
            value: EnumOrUnknown::from(MetricType::RATE),
        })
        .unwrap();
        assert_eq!(known["value"], serde_json::json!(2));

        let unknown = serde_json::to_value(EnumHolder {
            value: EnumOrUnknown::from_i32(99),
        })
        .unwrap();
        assert_eq!(unknown["value"], serde_json::json!(99));
    }

    #[test]
    fn deserialize_proto_enum_maps_each_integer_width_through_from_i32() {
        use serde::de::value::{Error as ValueError, I16Deserializer, I32Deserializer, U8Deserializer};

        // The hand-written visitor accepts several integer widths and routes them all through
        // `EnumOrUnknown::from_i32`, resolving known discriminants and preserving unknown ones. Each
        // case below is fed through the matching narrow-integer serde deserializer so the corresponding
        // `visit_*` arm is exercised directly.
        let u8_de: U8Deserializer<ValueError> = 1u8.into_deserializer();
        let from_u8: EnumOrUnknown<MetricType> =
            deserialize_proto_enum(u8_de).expect("u8 discriminant should deserialize");
        assert_eq!(from_u8.enum_value(), Ok(MetricType::COUNT));

        let i16_de: I16Deserializer<ValueError> = 3i16.into_deserializer();
        let from_i16: EnumOrUnknown<MetricType> =
            deserialize_proto_enum(i16_de).expect("i16 discriminant should deserialize");
        assert_eq!(from_i16.enum_value(), Ok(MetricType::GAUGE));

        let i32_de: I32Deserializer<ValueError> = 2i32.into_deserializer();
        let from_i32: EnumOrUnknown<MetricType> =
            deserialize_proto_enum(i32_de).expect("i32 discriminant should deserialize");
        assert_eq!(from_i32.enum_value(), Ok(MetricType::RATE));

        // An unrecognized discriminant is retained as `Unknown`, not rejected or clamped.
        let unknown_de: I32Deserializer<ValueError> = 99i32.into_deserializer();
        let unknown: EnumOrUnknown<MetricType> =
            deserialize_proto_enum(unknown_de).expect("unknown discriminant should still deserialize");
        assert_eq!(unknown.value(), 99);
        assert_eq!(unknown.enum_value(), Err(99));
    }

    #[test]
    fn proto_bytes_round_trip_preserves_the_exact_byte_sequence() {
        // The bytes helpers bridge protobuf's `Vec<u8>` fields to `serde_bytes`; a full round-trip must
        // reproduce every byte, including a zero and a high byte.
        let original = BytesHolder {
            data: vec![0, 1, 2, 254, 255],
        };
        let encoded = serde_json::to_value(&original).unwrap();
        let decoded: BytesHolder = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.data, vec![0, 1, 2, 254, 255]);
    }
}
