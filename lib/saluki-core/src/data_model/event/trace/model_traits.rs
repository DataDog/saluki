//! Trait implementations that let Saluki's trace types be used by shared trace-processing logic.
//!
//! The traits describe what a span, span event, span link and trace chunk are, without saying how they are stored.
//! Logic written against them (obfuscation, tag replacement, trace filtering) therefore runs directly on [`Span`],
//! [`SpanEvent`], [`SpanLink`] and [`Trace`], with no conversion into another representation and no per-type copy of the
//! logic.
//!
//! The write traits hand out mutable references to the attribute value type, so Saluki stores that type, parameterized
//! with its own string type ([`MetaString`]). See [`SalukiValues`].
//!
//! # Promoted tags
//!
//! The traits expect these tags to live only in their dedicated fields, never in the attribute map: `env`, `version`,
//! `component` and `span.kind` on spans, and the origin, sampling priority and sampling mechanism on traces. The
//! attribute getters and setters implemented here never touch those fields.

use std::sync::Arc;

use libdd_trace_model as model;
use libdd_trace_model::{AttributeRef, ValueMap, ValueMapMut};
use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

use super::{Span, SpanEvent, SpanLink, Trace};

/// Selects the concrete types behind attribute values: text is a [`MetaString`], bytes are a `Vec<u8>`, arrays are a
/// `Vec<AttributeValue>`, and key/value lists are a [`KeyValueList`].
///
/// It is only used as a type parameter, so it has no values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SalukiValues {}

/// Payload of [`AttributeValue::KeyValueList`].
pub type KeyValueList = Vec<(MetaString, AttributeValue)>;

impl model::ValueTypes for SalukiValues {
    type Text = MetaString;
    type Bytes = Vec<u8>;
    type Array = Vec<AttributeValue>;
    type Map = KeyValueList;
}

/// Typed value for attributes at every level of the trace model.
pub type AttributeValue = model::AttributeValue<SalukiValues>;

/// Returns `true` if a later list entry has the same key.
fn is_shadowed(entries: &KeyValueList, index: usize) -> bool {
    let key = &entries[index].0;
    entries[index + 1..].iter().any(|(later, _)| later == key)
}

/// Keep only each key's last entry, preserving the order of the surviving entries.
fn remove_shadowed(entries: &mut KeyValueList) {
    let mut index = 0;
    while index < entries.len() {
        if is_shadowed(entries, index) {
            Vec::remove(entries, index);
        } else {
            index += 1;
        }
    }
}

/// Key/value lists preserve decoded order and can contain duplicate keys; the last value wins.
impl ValueMap<SalukiValues> for KeyValueList {
    fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.as_slice()
            .iter()
            .rev()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v)
    }
    fn len(&self) -> usize {
        (0..self.as_slice().len()).filter(|&i| !is_shadowed(self, i)).count()
    }
    fn iter<'a>(&'a self) -> impl Iterator<Item = (&'a str, &'a AttributeValue)>
    where
        SalukiValues: 'a,
    {
        self.as_slice()
            .iter()
            .enumerate()
            .filter(|(i, _)| !is_shadowed(self, *i))
            .map(|(_, (k, v))| (k.as_ref(), v))
    }
}

impl ValueMapMut<SalukiValues> for KeyValueList {
    fn get_mut(&mut self, key: &str) -> Option<&mut AttributeValue> {
        self.as_mut_slice()
            .iter_mut()
            .rev()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v)
    }
    fn insert(&mut self, key: MetaString, value: AttributeValue) -> Option<AttributeValue> {
        match self
            .as_mut_slice()
            .iter_mut()
            .rev()
            .find(|(k, _)| k.as_ref() == key.as_ref())
        {
            Some((_, current)) => Some(std::mem::replace(current, value)),
            None => {
                self.push((key, value));
                None
            }
        }
    }
    fn remove(&mut self, key: &str) -> Option<AttributeValue> {
        let last = self.as_slice().iter().rposition(|(k, _)| k.as_ref() == key)?;
        let (_, value) = Vec::remove(self, last);
        self.retain(|(k, _)| &**k != key);
        Some(value)
    }
    fn retain(&mut self, mut f: impl FnMut(&str, &mut AttributeValue) -> bool) {
        remove_shadowed(self);
        Vec::retain_mut(self, |(k, v)| f(k.as_ref(), v));
    }
    fn iter_mut<'a>(&'a mut self) -> impl Iterator<Item = (&'a str, &'a mut AttributeValue)>
    where
        SalukiValues: 'a,
    {
        remove_shadowed(self);
        self.as_mut_slice().iter_mut().map(|(k, v)| {
            let k: &'a MetaString = k;
            (k.as_ref(), v)
        })
    }
}

/// Saluki's accessors for [`AttributeValue`].
///
/// [`AttributeValue`] is a type alias for a generic type from another crate, so these accessors are on a trait. Import
/// the trait to call them.
pub trait AttributeValueExt {
    /// Returns the inner string if this is a `String` variant.
    fn as_string(&self) -> Option<&MetaString>;

    /// Returns the inner float if this is a `Float` variant.
    ///
    /// Returns `Some` only when the stored variant is `Float`. For numeric semantic tags where the source may store an
    /// integer (for example, sampling priority, `_sample_rate`, `_top_level`), use [`as_num`][Self::as_num] instead.
    fn as_float(&self) -> Option<f64>;

    /// Returns the inner bool if this is a `Bool` variant.
    fn as_bool(&self) -> Option<bool>;

    /// Returns the inner integer if this is an `Int` variant.
    fn as_int(&self) -> Option<i64>;

    /// Returns a numeric value as `f64` for either `Float` or `Int` variants.
    ///
    /// Use this when the caller only needs a number and doesn't care whether the stored type is integral or
    /// floating-point (for example, sampling priority).
    fn as_num(&self) -> Option<f64>;

    /// Returns the inner bytes if this is a `Bytes` variant.
    fn as_bytes(&self) -> Option<&[u8]>;
}

impl AttributeValueExt for AttributeValue {
    fn as_string(&self) -> Option<&MetaString> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
    fn as_float(&self) -> Option<f64> {
        if let Self::Float(f) = self {
            Some(*f)
        } else {
            None
        }
    }
    fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }
    fn as_int(&self) -> Option<i64> {
        if let Self::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }
    fn as_num(&self) -> Option<f64> {
        self.as_f64()
    }
    fn as_bytes(&self) -> Option<&[u8]> {
        if let Self::Bytes(b) = self {
            Some(b)
        } else {
            None
        }
    }
}

type AttrMap = FastHashMap<MetaString, AttributeValue>;

fn str_of<'a>(m: &'a AttrMap, k: &str) -> Option<&'a str> {
    m.get(k).and_then(|v| v.as_str())
}
fn f64_of(m: &AttrMap, k: &str) -> Option<f64> {
    m.get(k).and_then(|v| v.as_f64())
}
fn i64_of(m: &AttrMap, k: &str) -> Option<i64> {
    m.get(k).and_then(|v| v.as_i64())
}

macro_rules! impl_attributes {
    ($ty:ty, |$s:ident| $map:expr, |$m:ident| $map_mut:expr) => {
        impl model::Attributes for $ty {
            type Text = MetaString;
            type Bytes = Vec<u8>;
            type Values = SalukiValues;
            fn attribute_str(&self, key: &str) -> Option<&str> {
                let $s = self;
                str_of($map, key)
            }
            fn attribute_f64(&self, key: &str) -> Option<f64> {
                let $s = self;
                f64_of($map, key)
            }
            fn attribute_i64(&self, key: &str) -> Option<i64> {
                let $s = self;
                i64_of($map, key)
            }
            fn has_attribute(&self, key: &str) -> bool {
                let $s = self;
                $map.contains_key(key)
            }
        }
        impl model::AttributesIter for $ty {
            fn attributes(&self) -> impl Iterator<Item = (&str, AttributeRef<'_, SalukiValues>)> {
                let $s = self;
                $map.iter().map(|(k, v)| (k.as_ref(), AttributeRef::from(v)))
            }
        }
        impl model::AttributesMut for $ty {
            fn retain_attributes(&mut self, mut f: impl FnMut(&str, &mut AttributeValue) -> bool) {
                let $m = self;
                $map_mut.retain(|k, v| f(k.as_ref(), v));
            }
            fn attribute_mut(&mut self, key: &str) -> Option<&mut AttributeValue> {
                let $m = self;
                $map_mut.get_mut(key)
            }
            fn set_attribute(&mut self, key: MetaString, value: AttributeValue) {
                let $m = self;
                $map_mut.insert(key, value);
            }
            fn remove_attribute(&mut self, key: &str) -> Option<AttributeValue> {
                let $m = self;
                $map_mut.remove(key)
            }
        }
    };
}

impl_attributes!(Span, |s| &s.attributes, |m| &mut m.attributes);
impl_attributes!(SpanEvent, |s| &s.attributes, |m| &mut m.attributes);
impl_attributes!(SpanLink, |s| &s.attributes, |m| &mut m.attributes);
// Trace-level attributes are behind an `Arc`: every write is copy-on-write.
impl_attributes!(Trace, |s| &*s.attributes, |m| Arc::make_mut(&mut m.attributes));

fn non_empty(s: &MetaString) -> Option<&str> {
    (!s.is_empty()).then_some(s.as_ref())
}

fn start_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn duration_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(-1)
}

fn from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn span_kind_name(kind: u32) -> Option<&'static str> {
    match kind {
        1 => Some("internal"),
        2 => Some("server"),
        3 => Some("client"),
        4 => Some("producer"),
        5 => Some("consumer"),
        _ => None,
    }
}

fn span_kind_from_name(name: &str) -> u32 {
    match name {
        "internal" => 1,
        "server" => 2,
        "client" => 3,
        "producer" => 4,
        "consumer" => 5,
        _ => 0,
    }
}

impl model::SpanEvent for SpanEvent {
    fn name(&self) -> &str {
        &self.name
    }
    fn time_unix_nano(&self) -> u64 {
        self.time_unix_nano
    }
}

impl model::SpanLink for SpanLink {
    fn trace_id(&self) -> u128 {
        (u128::from(self.trace_id_high) << 64) | u128::from(self.trace_id)
    }
    fn span_id(&self) -> u64 {
        self.span_id
    }
    fn tracestate(&self) -> &str {
        &self.tracestate
    }
    fn flags(&self) -> u32 {
        self.flags & !model::SPAN_LINK_FLAGS_SET_SENTINEL
    }
}

impl model::Span for Span {
    type SpanEvent = SpanEvent;
    type SpanLink = SpanLink;
    fn service(&self) -> &str {
        &self.service
    }
    fn resource(&self) -> &str {
        &self.resource
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn typ(&self) -> &str {
        &self.span_type
    }
    fn span_id(&self) -> u64 {
        self.span_id
    }
    fn parent_id(&self) -> u64 {
        self.parent_id
    }
    fn start(&self) -> i64 {
        start_to_i64(self.start)
    }
    fn duration(&self) -> i64 {
        duration_to_i64(self.duration)
    }
    fn is_error(&self) -> bool {
        self.error != 0
    }
    fn span_kind(&self) -> Option<&str> {
        span_kind_name(self.kind)
    }
    fn env(&self) -> Option<&str> {
        non_empty(&self.env)
    }
    fn version(&self) -> Option<&str> {
        non_empty(&self.version)
    }
    fn component(&self) -> Option<&str> {
        non_empty(&self.component)
    }
    fn span_events(&self) -> impl Iterator<Item = &SpanEvent> {
        self.span_events.iter()
    }
    fn span_links(&self) -> impl Iterator<Item = &SpanLink> {
        self.span_links.iter()
    }
}

impl model::SpanMut for Span {
    fn set_service(&mut self, value: MetaString) {
        self.service = value;
    }
    fn set_resource(&mut self, value: MetaString) {
        self.resource = value;
    }
    fn set_name(&mut self, value: MetaString) {
        self.name = value;
    }
    fn set_type(&mut self, value: MetaString) {
        self.span_type = value;
    }
    fn set_parent_id(&mut self, value: u64) {
        self.parent_id = value;
    }
    fn set_start(&mut self, value: i64) {
        self.start = from_i64(value);
    }
    fn set_duration(&mut self, value: i64) {
        self.duration = from_i64(value);
    }
    fn set_error(&mut self, value: bool) {
        self.error = i32::from(value);
    }
    fn set_span_kind(&mut self, value: &str) {
        self.kind = span_kind_from_name(value);
    }
    fn set_env(&mut self, value: MetaString) {
        self.env = value;
    }
    fn set_version(&mut self, value: MetaString) {
        self.version = value;
    }
    fn set_component(&mut self, value: MetaString) {
        self.component = value;
    }
    fn span_events_mut(&mut self) -> impl Iterator<Item = &mut SpanEvent> {
        self.span_events.iter_mut()
    }
    fn span_links_mut(&mut self) -> impl Iterator<Item = &mut SpanLink> {
        self.span_links.iter_mut()
    }
}

impl model::TraceChunk for Trace {
    type Span = Span;
    fn trace_id(&self) -> u128 {
        (u128::from(self.trace_id_high) << 64) | u128::from(self.trace_id_low)
    }
    fn origin(&self) -> &str {
        &self.origin
    }
    fn spans(&self) -> impl Iterator<Item = &Span> {
        self.spans.iter()
    }
    fn priority(&self) -> Option<i32> {
        self.priority
    }
    fn sampling_mechanism(&self) -> Option<u32> {
        // Saluki carries both a numeric mechanism and a `decision_maker` string.
        (self.sampling_mechanism != 0).then_some(self.sampling_mechanism)
    }
    fn dropped_trace(&self) -> bool {
        self.dropped_trace
    }
}

impl model::TraceChunkMut for Trace {
    fn set_origin(&mut self, value: MetaString) {
        self.origin = value;
    }
    fn set_priority(&mut self, priority: i32) {
        self.priority = Some(priority);
    }
    fn set_sampling_mechanism(&mut self, mechanism: u32) {
        self.sampling_mechanism = mechanism;
    }
    fn set_dropped_trace(&mut self, value: bool) {
        self.dropped_trace = value;
    }
    fn spans_mut(&mut self) -> impl Iterator<Item = &mut Span> {
        self.spans.iter_mut()
    }
    fn retain_spans(&mut self, mut f: impl FnMut(&mut Span) -> bool) {
        // Saluki's own `retain_spans` keeps the trace untouched when nothing matches; this does not.
        self.spans.retain_mut(|s| f(s));
    }
}

#[cfg(test)]
mod tests {
    use model::{Attributes as _, AttributesIter as _, AttributesMut as _, Span as _};

    use super::*;
    use crate::data_model::event::trace::TraceIngestInfo;

    fn string(value: &str) -> AttributeValue {
        AttributeValue::String(value.into())
    }

    /// Fails to compile if one of the traits that shared trace logic relies on stops being implemented.
    #[test]
    fn implements_the_shared_trace_traits() {
        fn assert_span<S: model::SpanMut>() {}
        fn assert_chunk<C: model::TraceChunkMut>() {}
        fn assert_iter<A: model::AttributesIter>() {}

        assert_span::<Span>();
        assert_chunk::<Trace>();
        assert_iter::<SpanEvent>();
        assert_iter::<SpanLink>();
    }

    #[test]
    fn span_kind_maps_otel_kinds_and_unsets_unknown_values() {
        let kinds = [
            (0, None),
            (1, Some("internal")),
            (2, Some("server")),
            (3, Some("client")),
            (4, Some("producer")),
            (5, Some("consumer")),
        ];
        for (kind, expected) in kinds {
            let span = Span::default().with_kind(kind);
            assert_eq!(span.span_kind(), expected, "kind {kind}");
        }

        let mut span = Span::default();
        model::SpanMut::set_span_kind(&mut span, "internal");
        assert_eq!(span.kind, 1);
        model::SpanMut::set_span_kind(&mut span, "not-a-kind");
        assert_eq!(span.kind, 0, "an unrecognized kind unsets it");
    }

    #[test]
    fn trait_time_fields_handle_values_outside_the_signed_range() {
        let mut span = Span::default().with_start(u64::MAX).with_duration(u64::MAX);
        assert_eq!(
            (model::Span::start(&span), model::Span::duration(&span)),
            (i64::MAX, -1)
        );

        model::SpanMut::set_start(&mut span, -1);
        model::SpanMut::set_duration(&mut span, -1);
        assert_eq!((model::Span::start(&span), model::Span::duration(&span)), (0, 0));
    }

    #[test]
    fn promoted_fields_read_as_unset_when_empty() {
        let span = Span::default();
        assert_eq!((span.env(), span.version(), span.component()), (None, None, None));

        let span = Span::default()
            .with_env("prod")
            .with_version("1.2")
            .with_component("net/http");
        assert_eq!(
            (span.env(), span.version(), span.component()),
            (Some("prod"), Some("1.2"), Some("net/http"))
        );
    }

    #[test]
    fn promoted_tags_are_never_read_from_or_written_to_the_attribute_map() {
        let mut span = Span::default().with_env("prod");
        assert_eq!(span.attribute_str("env"), None, "getters only see the attribute map");

        model::SpanMut::set_env(&mut span, "staging".into());
        assert_eq!(span.env(), Some("staging"));
        assert!(
            span.attributes.is_empty(),
            "setting a promoted field doesn't create an attribute"
        );

        // The compat view is what bridges the two.
        assert_eq!(span.compat().attribute_str("env"), Some("staging"));
    }

    #[test]
    fn span_fields_round_trip_through_the_traits() {
        let mut span = Span::new("svc", "op", "res", "web", 7, 3, 1_000, 50, 1);
        assert_eq!(
            (span.service(), span.name(), span.resource(), span.typ()),
            ("svc", "op", "res", "web")
        );
        assert_eq!(
            (span.span_id(), span.parent_id(), span.start(), span.duration()),
            (7, 3, 1_000, 50)
        );
        assert!(span.is_error());
        assert!(!span.is_trace_root());

        model::SpanMut::set_resource(&mut span, "new".into());
        model::SpanMut::set_type(&mut span, "sql".into());
        model::SpanMut::set_start(&mut span, 2_000);
        model::SpanMut::set_duration(&mut span, 75);
        model::SpanMut::set_error(&mut span, false);
        model::SpanMut::set_parent_id(&mut span, 0);
        assert_eq!(
            (span.resource(), span.typ(), span.start(), span.duration()),
            ("new", "sql", 2_000, 75)
        );
        assert!(!span.is_error());
        assert!(span.is_trace_root());
    }

    #[test]
    fn attributes_read_write_remove_and_retain() {
        let mut span = Span::default();
        span.set_attribute("a".into(), string("x"));
        span.set_attribute("n".into(), AttributeValue::Int(2));
        span.set_attribute("f".into(), AttributeValue::Float(2.5));
        span.set_attribute("nested".into(), AttributeValue::Array(vec![string("y")]));

        assert_eq!(span.attribute_str("a"), Some("x"));
        assert_eq!(span.attribute_str("n"), None, "a number isn't a string");
        assert_eq!(span.attribute_f64("n"), Some(2.0));
        assert_eq!(span.attribute_i64("n"), Some(2));
        assert_eq!(span.attribute_i64("f"), None, "2.5 isn't exactly an integer");
        assert!(span.has_attribute("nested"));
        assert!(!span.has_attribute("missing"));

        if let Some(AttributeValue::String(s)) = span.attribute_mut("a") {
            *s = "changed".into();
        }
        assert_eq!(span.attribute_str("a"), Some("changed"));

        assert_eq!(span.remove_attribute("a"), Some(string("changed")));
        assert_eq!(span.remove_attribute("a"), None);

        span.retain_attributes(|key, _| key != "n");
        assert!(!span.has_attribute("n"));
        assert!(span.has_attribute("f"));
    }

    #[test]
    fn iterating_attributes_yields_each_key_once() {
        let mut span = Span::default();
        span.set_attribute("a".into(), string("x"));
        span.set_attribute("b".into(), AttributeValue::Bool(true));
        span.set_attribute("c".into(), AttributeValue::Array(vec![string("y")]));

        let mut keys: Vec<_> = span.attributes().map(|(k, _)| k.to_string()).collect();
        keys.sort();
        assert_eq!(keys, ["a", "b", "c"]);
    }

    #[test]
    fn key_value_lists_support_the_map_traits() {
        let mut list: KeyValueList = Vec::new();
        assert_eq!(ValueMapMut::insert(&mut list, "k".into(), string("v")), None);
        assert_eq!(
            ValueMapMut::insert(&mut list, "k".into(), string("w")),
            Some(string("v")),
            "insert replaces"
        );
        assert_eq!(list.len(), 1);
        assert_eq!(ValueMap::get(&list, "k"), Some(&string("w")));
        assert_eq!(ValueMapMut::remove(&mut list, "k"), Some(string("w")));
        assert!(ValueMap::is_empty(&list));
    }

    #[test]
    fn key_value_lists_treat_the_last_duplicate_as_the_value() {
        let mut list = vec![
            ("a".into(), string("first")),
            ("b".into(), string("only")),
            ("a".into(), string("last")),
        ];

        assert_eq!(ValueMap::get(&list, "a"), Some(&string("last")));
        assert_eq!(ValueMap::len(&list), 2);
        let entries: Vec<_> = ValueMap::iter(&list).map(|(k, v)| (k, v.as_str().unwrap())).collect();
        assert_eq!(entries, [("b", "only"), ("a", "last")]);

        let mut visited = Vec::new();
        ValueMapMut::retain(&mut list, |key, _| {
            visited.push(key.to_owned());
            key != "b"
        });
        assert_eq!(visited, ["b", "a"]);
        assert_eq!(list.len(), 1);
        assert_eq!(ValueMapMut::remove(&mut list, "a"), Some(string("last")));
        assert!(list.is_empty());
    }

    #[test]
    fn span_links_combine_trace_id_halves_and_mask_the_flags_sentinel() {
        let link = SpanLink::new(1, 9)
            .with_trace_id_high(2)
            .with_flags(model::SPAN_LINK_FLAGS_SET_SENTINEL | 1);
        assert_eq!(model::SpanLink::trace_id(&link), (2u128 << 64) | 1);
        assert_eq!(model::SpanLink::flags(&link), 1);
    }

    #[test]
    fn trace_attributes_are_copy_on_write() {
        let mut first = Trace::new(Vec::new(), TraceIngestInfo::Datadog);
        first.set_attribute("k".into(), string("v"));
        let mut second = first.clone();

        second.set_attribute("k".into(), string("changed"));
        assert_eq!(
            first.attribute_str("k"),
            Some("v"),
            "the clone's write must not reach the original"
        );
        assert_eq!(second.attribute_str("k"), Some("changed"));
    }

    #[test]
    fn trace_chunk_fields_and_span_retention() {
        let spans = vec![
            Span::default().with_resource("keep"),
            Span::default().with_resource("drop"),
        ];
        let mut trace = Trace::new(spans, TraceIngestInfo::Datadog);
        trace.trace_id_high = 1;
        trace.trace_id_low = 2;

        assert_eq!(model::TraceChunk::trace_id(&trace), (1u128 << 64) | 2);
        assert_eq!(model::TraceChunk::priority(&trace), None);
        assert_eq!(
            model::TraceChunk::sampling_mechanism(&trace),
            None,
            "0 means no mechanism"
        );

        model::TraceChunkMut::set_priority(&mut trace, 2);
        model::TraceChunkMut::set_sampling_mechanism(&mut trace, 4);
        model::TraceChunkMut::set_origin(&mut trace, "lambda".into());
        model::TraceChunkMut::set_dropped_trace(&mut trace, true);
        assert_eq!(
            (
                model::TraceChunk::priority(&trace),
                model::TraceChunk::sampling_mechanism(&trace),
                model::TraceChunk::origin(&trace)
            ),
            (Some(2), Some(4), "lambda")
        );
        assert!(model::TraceChunk::dropped_trace(&trace));

        model::TraceChunkMut::retain_spans(&mut trace, |span| model::Span::resource(span) == "keep");
        assert_eq!(trace.spans().len(), 1);
    }
}
