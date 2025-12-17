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

        fn visit_i32<R>(self, v: i32) -> Result<Self::Value, R>
        where
            R: serde::de::Error,
        {
            Ok(EnumOrUnknown::from_i32(v))
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
    Vec::<T>::deserialize(d).map(Vec::from)
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
