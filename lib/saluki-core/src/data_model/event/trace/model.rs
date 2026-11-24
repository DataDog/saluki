//! Implementations of the `libdd-trace-model` traits for the trace types.
//!
//! The traits let `libdd-trace-*` crates, such as the trace normalizer, read and change a [`Trace`] in place. The
//! mapping between the fields and the traits:
//!
//! - The attribute accessors reach only the attribute maps. The fields of the promoted tags (`env`, `version`,
//!   `component`, and the span kind on a span; the origin, the priority, and the sampling mechanism on a trace) have
//!   their own accessors, as the `Attributes` trait documents.
//! - An empty `env`, `version`, or `component` is unset.
//! - The span kind is the OTel value: 0 is unset, and an unknown value has no name.
//! - A sampling mechanism of 0 is unset.
//! - The trace attributes are shared, so a write to them clones the map if another trace holds it.
//! - Start and duration are `u64` here and `i64` in the traits. See [`to_i64`].

use std::sync::Arc;

use libdd_trace_model as model;
use stringtheory::MetaString;

use super::{AttributeValue, SalukiValues, Span, SpanEvent, SpanLink, Trace};

/// Returns `true` if a later entry of `entries` has the key of the entry at `index`.
fn is_shadowed(entries: &[(MetaString, AttributeValue)], index: usize) -> bool {
    let key = &entries[index].0;
    entries[index + 1..].iter().any(|(later, _)| later == key)
}

/// Removes the entries that a later entry with the same key shadows, and keeps the order of the others.
fn remove_shadowed(entries: &mut Vec<(MetaString, AttributeValue)>) {
    let mut index = 0;
    while index < entries.len() {
        if is_shadowed(entries, index) {
            Vec::remove(entries, index);
        } else {
            index += 1;
        }
    }
}

/// A key-value list in decoded order.
///
/// A list can hold the same key more than once. The last entry for a key shadows the earlier ones, so lookups return
/// it, and iteration skips the earlier ones. Lists are short, so the checks scan the list.
impl model::ValueMap<SalukiValues> for Vec<(MetaString, AttributeValue)> {
    fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.as_slice().iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn len(&self) -> usize {
        (0..self.as_slice().len()).filter(|i| !is_shadowed(self, *i)).count()
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

impl model::ValueMapMut<SalukiValues> for Vec<(MetaString, AttributeValue)> {
    fn get_mut(&mut self, key: &str) -> Option<&mut AttributeValue> {
        self.as_mut_slice()
            .iter_mut()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn insert(&mut self, key: MetaString, value: AttributeValue) -> Option<AttributeValue> {
        match self.as_mut_slice().iter_mut().rev().find(|(k, _)| *k == key) {
            Some((_, current)) => Some(std::mem::replace(current, value)),
            None => {
                self.push((key, value));
                None
            }
        }
    }

    fn remove(&mut self, key: &str) -> Option<AttributeValue> {
        let last = self.as_slice().iter().rposition(|(k, _)| k == key)?;
        let (_, value) = Vec::remove(self, last);
        Vec::retain(self, |(k, _)| k != key);
        Some(value)
    }

    fn retain(&mut self, mut f: impl FnMut(&str, &mut AttributeValue) -> bool) {
        remove_shadowed(self);
        self.retain_mut(|(k, v)| f(k, v));
    }

    fn iter_mut<'a>(&'a mut self) -> impl Iterator<Item = (&'a str, &'a mut AttributeValue)>
    where
        SalukiValues: 'a,
    {
        remove_shadowed(self);
        self.as_mut_slice().iter_mut().map(|(k, v)| {
            let k: &MetaString = k;
            (k.as_ref(), v)
        })
    }
}

/// Converts a start or a duration to the `i64` of the traits, with saturation to `i64::MAX`.
///
/// The V1 protocol encodes both as `u64`, and a value up to `i64::MAX` (until the year 2262) converts exactly. A start
/// above `i64::MAX` doesn't fit: the normalizer then sets a non-zero duration to 0, where the trace-agent keeps it. A
/// duration above `i64::MAX` is invalid, and both the normalizer and the trace-agent set it to 0.
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Converts a start or a duration from the `i64` of the traits. A negative value becomes 0.
fn from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Returns `text`, or `None` if it is empty.
fn non_empty(text: &MetaString) -> Option<&str> {
    Some(text.as_ref()).filter(|s| !s.is_empty())
}

/// Returns the `span.kind` name of an OTel span kind, or `None` if it is unset or unknown.
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

/// Returns the OTel span kind of a `span.kind` name. An unknown name gives 0, which is unset.
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

/// Implements the attribute traits for a type with an `attributes: FastHashMap<MetaString, AttributeValue>` field.
macro_rules! impl_attributes {
    ($($ty:ty),*) => {$(
        impl model::Attributes for $ty {
            type Text = MetaString;
            type Bytes = Vec<u8>;
            type Values = SalukiValues;

            fn attribute_str(&self, key: &str) -> Option<&str> {
                self.attributes.get(key)?.as_str()
            }

            fn attribute_f64(&self, key: &str) -> Option<f64> {
                self.attributes.get(key)?.as_f64()
            }

            fn attribute_i64(&self, key: &str) -> Option<i64> {
                self.attributes.get(key)?.as_i64()
            }

            fn has_attribute(&self, key: &str) -> bool {
                self.attributes.contains_key(key)
            }

            fn attribute(&self, key: &str) -> Option<model::AttributeRef<'_, SalukiValues>> {
                self.attributes.get(key).map(Into::into)
            }
        }

        impl model::AttributesIter for $ty {
            fn attributes(&self) -> impl Iterator<Item = (&str, model::AttributeRef<'_, SalukiValues>)> {
                self.attributes.iter().map(|(k, v)| (k.as_ref(), v.into()))
            }
        }
    )*};
}

impl_attributes!(Trace, Span, SpanLink, SpanEvent);

/// Implements `AttributesMut` for a type with an `attributes: FastHashMap<MetaString, AttributeValue>` field.
macro_rules! impl_attributes_mut {
    ($($ty:ty),*) => {$(
        impl model::AttributesMut for $ty {
            fn retain_attributes(&mut self, mut f: impl FnMut(&str, &mut AttributeValue) -> bool) {
                self.attributes.retain(|k, v| f(k, v));
            }

            fn attribute_mut(&mut self, key: &str) -> Option<&mut AttributeValue> {
                self.attributes.get_mut(key)
            }

            fn set_attribute(&mut self, key: MetaString, value: AttributeValue) {
                self.attributes.insert(key, value);
            }

            fn remove_attribute(&mut self, key: &str) -> Option<AttributeValue> {
                self.attributes.remove(key)
            }
        }
    )*};
}

impl_attributes_mut!(Span, SpanLink, SpanEvent);

/// Clones the trace attributes only if another trace shares them and the write changes them.
impl model::AttributesMut for Trace {
    fn retain_attributes(&mut self, f: impl FnMut(&str, &mut AttributeValue) -> bool) {
        if self.attributes.is_empty() {
            return;
        }
        let mut f = f;
        Arc::make_mut(&mut self.attributes).retain(|k, v| f(k, v));
    }

    fn attribute_mut(&mut self, key: &str) -> Option<&mut AttributeValue> {
        if !self.attributes.contains_key(key) {
            return None;
        }
        Arc::make_mut(&mut self.attributes).get_mut(key)
    }

    fn set_attribute(&mut self, key: MetaString, value: AttributeValue) {
        Arc::make_mut(&mut self.attributes).insert(key, value);
    }

    fn remove_attribute(&mut self, key: &str) -> Option<AttributeValue> {
        if !self.attributes.contains_key(key) {
            return None;
        }
        Arc::make_mut(&mut self.attributes).remove(key)
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
        to_i64(self.start)
    }

    fn duration(&self) -> i64 {
        to_i64(self.duration)
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
        Some(self.sampling_mechanism).filter(|mechanism| *mechanism != 0)
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

    fn retain_spans(&mut self, f: impl FnMut(&mut Span) -> bool) {
        self.spans.retain_mut(f);
    }
}

#[cfg(test)]
mod tests;
