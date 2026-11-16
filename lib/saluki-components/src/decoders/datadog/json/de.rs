//! Deserialization of a JSON `pb.Traces` value into the classic tracer payload model, as Go's `encoding/json` decodes
//! it.
//!
//! The classic protos derive `serde`, but under their protobuf field names (`TraceID`, `SpanID`) and with `serde`'s
//! own semantics, which aren't Go's. So the values are decoded here with visitors that follow `encoding/json`
//! (`decode.go`):
//!
//! - Object keys match field names (the `json` tags of `pb.Span` and friends) case-insensitively, folding as Go's
//!   `foldName` does, so `ſ` (U+017F) also matches `s` and `K` (U+212A, the Kelvin sign) matches `k`. Unknown keys are
//!   skipped.
//! - `null` leaves a string, number or boolean field as it was, and empties a map, list or nested object.
//! - A key that appears twice decodes into what the first one decoded: the last value of a field wins, maps merge, and
//!   list elements decode into the elements already there. Elements left over from a longer earlier list aren't
//!   reused, where Go's `reflect` growth can resurrect some of them on a third occurrence of the key.
//! - Map values start from zero for each key, so `null` is `""` in `meta` and `0` in `metrics`.
//! - Integers parse as `strconv.ParseInt` and `ParseUint` parse the literal: a fraction, an exponent or an
//!   out-of-range value is a type error, as is `-0` in an unsigned field. Floats parse to the nearest value, and one
//!   out of range is a type error.
//! - `meta_struct` values (`[]byte`) are standard base64 with padding, ignoring line breaks, or arrays of bytes.
//! - Lone surrogate escapes become U+FFFD. Invalid UTF-8 is repaired before decoding, one U+FFFD per byte.
//! - `null` spans, span links, span events and attribute values are dropped, as `ConvertToIdx` drops them, and traces
//!   without spans are dropped as it drops chunks without spans.
//!
//! A type error ends decoding, where Go carries on and reports it at the end: either way the payload is rejected.

use std::collections::HashMap;
use std::fmt;

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use datadog_protos::traces::{
    AttributeAnyValue, AttributeArray, AttributeArrayValue, Span, SpanEvent, SpanLink, TraceChunk,
};
use protobuf::{EnumOrUnknown, MessageField};
use serde::de::{self, DeserializeSeed, Deserializer, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize as _;
use serde_json::value::RawValue;

use crate::decoders::datadog::v04::PRIORITY_NONE;

/// Go's `base64.StdEncoding`, which requires padding but not zeroed trailing bits.
const STD_ENCODING: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// Decodes a JSON `pb.Traces` value into one chunk per trace.
///
/// The input must be one syntactically valid JSON value in valid UTF-8, so that any error is a type error.
pub(super) fn decode_traces(json: &[u8]) -> Result<Vec<TraceChunk>, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    deserializer.deserialize_option(TracesVisitor)
}

struct TracesVisitor;

impl<'de> Visitor<'de> for TracesVisitor {
    type Value = Vec<TraceChunk>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a list of traces")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut chunks = Vec::new();
        while let Some(spans) = seq.next_element_seed(TraceSeed)? {
            if !spans.is_empty() {
                // As `traceChunksFromTraces` builds them.
                chunks.push(TraceChunk {
                    priority: PRIORITY_NONE,
                    spans,
                    ..TraceChunk::default()
                });
            }
        }
        Ok(chunks)
    }
}

/// One trace, a list of spans, `null` being an empty one.
struct TraceSeed;

impl<'de> DeserializeSeed<'de> for TraceSeed {
    type Value = Vec<Span>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for TraceSeed {
    type Value = Vec<Span>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a list of spans")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut spans = Vec::new();
        loop {
            let mut span = None;
            if seq.next_element_seed(PointerInto::<SpanFields>(&mut span))?.is_none() {
                return Ok(spans);
            }
            if let Some(span) = span {
                spans.push(span.finish());
            }
        }
    }
}

/// A Go struct that a JSON object decodes into, field by field.
trait Fields: Default {
    /// The fields' JSON names, as their `json` tags spell them.
    const NAMES: &'static [&'static str];

    /// Decodes the value of the field named `name`, one of [`NAMES`](Fields::NAMES), into `self`.
    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error>;
}

/// Decodes a JSON object into an existing struct.
struct FieldsVisitor<'a, T>(&'a mut T);

impl<'de, T: Fields> Visitor<'de> for FieldsVisitor<'_, T> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(field) = map.next_key_seed(FieldKey(T::NAMES))? {
            match field {
                Some(name) => self.0.decode_field(name, &mut map)?,
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

/// A pointer to a struct: `null` makes it `nil`, and an object decodes into the struct, allocated if it was `nil`.
struct PointerInto<'a, T>(&'a mut Option<Box<T>>);

impl<'de, T: Fields> DeserializeSeed<'de> for PointerInto<'_, T> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de, T: Fields> Visitor<'de> for PointerInto<'_, T> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        *self.0 = None;
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        let value = self.0.get_or_insert_with(Box::default);
        deserializer.deserialize_map(FieldsVisitor(&mut **value))
    }
}

/// A list of pointers to structs, decoded into the elements already there.
///
/// `nil` elements are kept as one pointer each until the enclosing struct is finished, so that `null` costs no more
/// here than it does in Go.
struct ListInto<'a, T>(&'a mut Vec<Option<Box<T>>>);

impl<'de, T: Fields> DeserializeSeed<'de> for ListInto<'_, T> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de, T: Fields> Visitor<'de> for ListInto<'_, T> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a list or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        self.0.clear();
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut len = 0;
        loop {
            if len == self.0.len() {
                self.0.push(None);
            }
            if seq.next_element_seed(PointerInto(&mut self.0[len]))?.is_none() {
                break;
            }
            len += 1;
        }
        self.0.truncate(len);
        Ok(())
    }
}

/// Drops the `nil` elements of a decoded list.
fn non_nil<T>(list: Vec<Option<Box<T>>>) -> Vec<T> {
    list.into_iter().flatten().map(|value| *value).collect()
}

/// A `map[string]V`: `null` makes it `nil`, and an object adds its entries to it.
struct MapInto<'a, V>(&'a mut HashMap<String, V>);

impl<'de, V: MapValue> DeserializeSeed<'de> for MapInto<'_, V> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de, V: MapValue> Visitor<'de> for MapInto<'_, V> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        self.0.clear();
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key_seed(KeyString)? {
            V::decode_entry(&mut map, key, self.0)?;
        }
        Ok(())
    }
}

/// A map value type, decoded from zero for each key.
trait MapValue: Sized {
    fn decode_entry<'de, A: MapAccess<'de>>(
        map: &mut A, key: String, target: &mut HashMap<String, Self>,
    ) -> Result<(), A::Error>;
}

impl MapValue for String {
    fn decode_entry<'de, A: MapAccess<'de>>(
        map: &mut A, key: String, target: &mut HashMap<String, Self>,
    ) -> Result<(), A::Error> {
        let mut value = String::new();
        map.next_value_seed(StringInto(&mut value))?;
        target.insert(key, value);
        Ok(())
    }
}

impl MapValue for f64 {
    fn decode_entry<'de, A: MapAccess<'de>>(
        map: &mut A, key: String, target: &mut HashMap<String, Self>,
    ) -> Result<(), A::Error> {
        let mut value = 0.0;
        map.next_value_seed(NumberInto(&mut value))?;
        target.insert(key, value);
        Ok(())
    }
}

impl MapValue for Vec<u8> {
    fn decode_entry<'de, A: MapAccess<'de>>(
        map: &mut A, key: String, target: &mut HashMap<String, Self>,
    ) -> Result<(), A::Error> {
        let mut value = Vec::new();
        map.next_value_seed(BytesInto(&mut value))?;
        target.insert(key, value);
        Ok(())
    }
}

impl MapValue for AttributeAnyValue {
    fn decode_entry<'de, A: MapAccess<'de>>(
        map: &mut A, key: String, target: &mut HashMap<String, Self>,
    ) -> Result<(), A::Error> {
        let mut value = None;
        map.next_value_seed(PointerInto::<AnyValueFields>(&mut value))?;
        // A `nil` value is dropped by the conversion, so it is dropped here, which also drops an earlier value.
        match value {
            Some(value) => target.insert(key, value.finish()),
            None => target.remove(&key),
        };
        Ok(())
    }
}

/// Matches an object key to the struct field it decodes into, if any.
struct FieldKey(&'static [&'static str]);

impl<'de> DeserializeSeed<'de> for FieldKey {
    type Value = Option<&'static str>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_bytes(self)
    }
}

impl<'de> Visitor<'de> for FieldKey {
    type Value = Option<&'static str>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a field name")
    }

    fn visit_bytes<E: de::Error>(self, key: &[u8]) -> Result<Self::Value, E> {
        Ok(self.0.iter().copied().find(|name| fold_equals(key, name)))
    }
}

/// Returns whether an object key names a field, as `encoding/json` compares them after `foldName`.
///
/// `foldName` maps each character to the smallest of its simple case folds. The only characters outside ASCII that
/// fold to an ASCII letter are `ſ` (to `S`) and `K` (to `K`); every other one can't match an ASCII name.
fn fold_equals(key: &[u8], name: &str) -> bool {
    let Ok(key) = simdutf8::basic::from_utf8(key) else {
        return false;
    };
    let mut chars = key.chars();
    for expected in name.bytes() {
        let folded = match chars.next() {
            Some('\u{017f}') => b'S',
            Some('\u{212a}') => b'K',
            Some(c) if c.is_ascii() => (c as u8).to_ascii_uppercase(),
            _ => return false,
        };
        if folded != expected.to_ascii_uppercase() {
            return false;
        }
    }
    chars.next().is_none()
}

/// A map key, as a string.
struct KeyString;

impl<'de> DeserializeSeed<'de> for KeyString {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        let mut key = String::new();
        deserializer.deserialize_bytes(StringBytes(&mut key))?;
        Ok(key)
    }
}

/// A `string` field.
struct StringInto<'a>(&'a mut String);

impl<'de> DeserializeSeed<'de> for StringInto<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for StringInto<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a string")
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        // Bytes rather than a string, so that lone surrogate escapes come through to be replaced rather than rejected.
        deserializer.deserialize_bytes(StringBytes(self.0))
    }
}

struct StringBytes<'a>(&'a mut String);

impl<'de> Visitor<'de> for StringBytes<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a string")
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<(), E> {
        *self.0 = go_string(value);
        Ok(())
    }
}

/// Converts the bytes of a decoded string to a string, replacing each lone surrogate with U+FFFD as Go does.
///
/// `serde_json` writes a lone surrogate escape as the three bytes of its WTF-8 encoding (`ED A0..BF 80..BF`), which
/// are the only invalid UTF-8 a decoded string can hold once the input itself is valid UTF-8.
fn go_string(mut bytes: &[u8]) -> String {
    if let Ok(valid) = simdutf8::basic::from_utf8(bytes) {
        return valid.to_owned();
    }
    let mut out = String::with_capacity(bytes.len());
    loop {
        match simdutf8::compat::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(valid);
                return out;
            }
            Err(error) => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                out.push_str(simdutf8::basic::from_utf8(valid).expect("prefix is valid UTF-8"));
                out.push(char::REPLACEMENT_CHARACTER);
                let surrogate = rest.len() >= 3 && rest[0] == 0xed && (0xa0..=0xbf).contains(&rest[1]);
                bytes = &rest[if surrogate { 3 } else { 1 }..];
            }
        }
    }
}

/// A `[]byte` field: base64 text, or a list of bytes.
struct BytesInto<'a>(&'a mut Vec<u8>);

impl<'de> DeserializeSeed<'de> for BytesInto<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for BytesInto<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("base64 text or a list of bytes")
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        self.0.clear();
        Ok(())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_bytes(self)
    }

    fn visit_bytes<E: de::Error>(self, text: &[u8]) -> Result<(), E> {
        // `base64.StdEncoding` skips line breaks wherever they are.
        let text: Vec<u8> = text.iter().copied().filter(|c| !matches!(c, b'\r' | b'\n')).collect();
        *self.0 = STD_ENCODING.decode(&text).map_err(E::custom)?;
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        self.0.clear();
        loop {
            let mut byte = 0u8;
            if seq.next_element_seed(NumberInto(&mut byte))?.is_none() {
                return Ok(());
            }
            self.0.push(byte);
        }
    }
}

/// A Go number type, parsed from a JSON number literal as `encoding/json` parses it.
trait GoNumber: Sized {
    const NAME: &'static str;

    fn parse(literal: &str) -> Option<Self>;
}

macro_rules! go_integer {
    ($($ty:ty => $name:literal),*) => {
        $(impl GoNumber for $ty {
            const NAME: &'static str = $name;

            // `strconv.ParseInt` or `ParseUint` in base 10; a JSON literal never has the `+` that `from_str` allows.
            fn parse(literal: &str) -> Option<Self> {
                literal.parse().ok()
            }
        })*
    };
}

go_integer!(u8 => "uint8", i32 => "int32", u32 => "uint32", i64 => "int64", u64 => "uint64");

impl GoNumber for f64 {
    const NAME: &'static str = "float64";

    // `strconv.ParseFloat`, which rounds to the nearest value and fails on overflow.
    fn parse(literal: &str) -> Option<Self> {
        literal.parse::<f64>().ok().filter(|value| value.is_finite())
    }
}

/// A number field: `null` leaves it as it was.
struct NumberInto<'a, T>(&'a mut T);

impl<'de, T: GoNumber> DeserializeSeed<'de> for NumberInto<'_, T> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        // The raw literal, since `serde_json` reads numbers as `u64`, `i64` or an approximate `f64`.
        let raw = <&RawValue>::deserialize(deserializer)?;
        match raw.get() {
            "null" => Ok(()),
            literal if literal.starts_with(['-', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9']) => {
                *self.0 = T::parse(literal).ok_or_else(|| {
                    D::Error::custom(format_args!("cannot unmarshal number {literal} into a {}", T::NAME))
                })?;
                Ok(())
            }
            other => Err(type_error::<D::Error>(other, T::NAME)),
        }
    }
}

/// A `bool` field: `null` leaves it as it was.
struct BoolInto<'a>(&'a mut bool);

impl<'de> DeserializeSeed<'de> for BoolInto<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        match <&RawValue>::deserialize(deserializer)?.get() {
            "null" => {}
            "true" => *self.0 = true,
            "false" => *self.0 = false,
            other => return Err(type_error::<D::Error>(other, "bool")),
        }
        Ok(())
    }
}

/// The error for a JSON value of the wrong type, as `encoding/json`'s `UnmarshalTypeError` describes it.
fn type_error<E: de::Error>(raw: &str, into: &str) -> E {
    let kind = match raw.as_bytes().first() {
        Some(b'"') => "string",
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b't' | b'f') => "bool",
        _ => "number",
    };
    E::custom(format_args!("cannot unmarshal {kind} into a {into}"))
}

/// `pb.Span`, with its span links and events kept as pointers until the span is finished.
#[derive(Default)]
struct SpanFields {
    span: Span,
    links: Vec<Option<Box<SpanLink>>>,
    events: Vec<Option<Box<SpanEvent>>>,
}

impl SpanFields {
    fn finish(self) -> Span {
        let mut span = self.span;
        span.spanLinks = non_nil(self.links);
        span.spanEvents = non_nil(self.events);
        span
    }
}

impl Fields for SpanFields {
    const NAMES: &'static [&'static str] = &[
        "service",
        "name",
        "resource",
        "trace_id",
        "span_id",
        "parent_id",
        "start",
        "duration",
        "error",
        "meta",
        "metrics",
        "type",
        "meta_struct",
        "span_links",
        "span_events",
    ];

    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error> {
        let span = &mut self.span;
        match name {
            "service" => map.next_value_seed(StringInto(&mut span.service)),
            "name" => map.next_value_seed(StringInto(&mut span.name)),
            "resource" => map.next_value_seed(StringInto(&mut span.resource)),
            "trace_id" => map.next_value_seed(NumberInto(&mut span.traceID)),
            "span_id" => map.next_value_seed(NumberInto(&mut span.spanID)),
            "parent_id" => map.next_value_seed(NumberInto(&mut span.parentID)),
            "start" => map.next_value_seed(NumberInto(&mut span.start)),
            "duration" => map.next_value_seed(NumberInto(&mut span.duration)),
            "error" => map.next_value_seed(NumberInto(&mut span.error)),
            "meta" => map.next_value_seed(MapInto(&mut span.meta)),
            "metrics" => map.next_value_seed(MapInto(&mut span.metrics)),
            "type" => map.next_value_seed(StringInto(&mut span.type_)),
            "meta_struct" => map.next_value_seed(MapInto(&mut span.meta_struct)),
            "span_links" => map.next_value_seed(ListInto(&mut self.links)),
            "span_events" => map.next_value_seed(ListInto(&mut self.events)),
            _ => unreachable!("not a span field: {name}"),
        }
    }
}

impl Fields for SpanLink {
    const NAMES: &'static [&'static str] = &[
        "trace_id",
        "trace_id_high",
        "span_id",
        "attributes",
        "tracestate",
        "flags",
    ];

    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error> {
        match name {
            "trace_id" => map.next_value_seed(NumberInto(&mut self.traceID)),
            "trace_id_high" => map.next_value_seed(NumberInto(&mut self.traceID_high)),
            "span_id" => map.next_value_seed(NumberInto(&mut self.spanID)),
            "attributes" => map.next_value_seed(MapInto(&mut self.attributes)),
            "tracestate" => map.next_value_seed(StringInto(&mut self.tracestate)),
            "flags" => map.next_value_seed(NumberInto(&mut self.flags)),
            _ => unreachable!("not a span link field: {name}"),
        }
    }
}

impl Fields for SpanEvent {
    const NAMES: &'static [&'static str] = &["time_unix_nano", "name", "attributes"];

    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error> {
        match name {
            "time_unix_nano" => map.next_value_seed(NumberInto(&mut self.time_unix_nano)),
            "name" => map.next_value_seed(StringInto(&mut self.name)),
            "attributes" => map.next_value_seed(MapInto(&mut self.attributes)),
            _ => unreachable!("not a span event field: {name}"),
        }
    }
}

/// `pb.AttributeAnyValue`, with its array kept as a pointer until the value is finished.
#[derive(Default)]
struct AnyValueFields {
    value: AttributeAnyValue,
    array: Option<Box<ArrayFields>>,
}

impl AnyValueFields {
    fn finish(self) -> AttributeAnyValue {
        let mut value = self.value;
        value.array_value = MessageField::from_option(self.array.map(|array| AttributeArray {
            values: non_nil(array.values),
            ..AttributeArray::default()
        }));
        value
    }
}

impl Fields for AnyValueFields {
    const NAMES: &'static [&'static str] = &[
        "type",
        "string_value",
        "bool_value",
        "int_value",
        "double_value",
        "array_value",
    ];

    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error> {
        let value = &mut self.value;
        match name {
            "type" => {
                let mut value_type = value.type_.value();
                map.next_value_seed(NumberInto(&mut value_type))?;
                value.type_ = EnumOrUnknown::from_i32(value_type);
                Ok(())
            }
            "string_value" => map.next_value_seed(StringInto(&mut value.string_value)),
            "bool_value" => map.next_value_seed(BoolInto(&mut value.bool_value)),
            "int_value" => map.next_value_seed(NumberInto(&mut value.int_value)),
            "double_value" => map.next_value_seed(NumberInto(&mut value.double_value)),
            "array_value" => map.next_value_seed(PointerInto(&mut self.array)),
            _ => unreachable!("not an attribute value field: {name}"),
        }
    }
}

/// `pb.AttributeArray`.
#[derive(Default)]
struct ArrayFields {
    values: Vec<Option<Box<AttributeArrayValue>>>,
}

impl Fields for ArrayFields {
    const NAMES: &'static [&'static str] = &["values"];

    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error> {
        match name {
            "values" => map.next_value_seed(ListInto(&mut self.values)),
            _ => unreachable!("not an attribute array field: {name}"),
        }
    }
}

impl Fields for AttributeArrayValue {
    const NAMES: &'static [&'static str] = &["type", "string_value", "bool_value", "int_value", "double_value"];

    fn decode_field<'de, A: MapAccess<'de>>(&mut self, name: &'static str, map: &mut A) -> Result<(), A::Error> {
        match name {
            "type" => {
                let mut value_type = self.type_.value();
                map.next_value_seed(NumberInto(&mut value_type))?;
                self.type_ = EnumOrUnknown::from_i32(value_type);
                Ok(())
            }
            "string_value" => map.next_value_seed(StringInto(&mut self.string_value)),
            "bool_value" => map.next_value_seed(BoolInto(&mut self.bool_value)),
            "int_value" => map.next_value_seed(NumberInto(&mut self.int_value)),
            "double_value" => map.next_value_seed(NumberInto(&mut self.double_value)),
            _ => unreachable!("not an attribute array value field: {name}"),
        }
    }
}
