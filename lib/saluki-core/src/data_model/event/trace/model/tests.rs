use std::sync::Arc;

use libdd_trace_model::{
    AttributeRef, Attributes as _, AttributesMut as _, Span as _, SpanMut, TraceChunk as _, TraceChunkMut as _,
    ValueMap, ValueMapMut, SPAN_LINK_FLAGS_SET_SENTINEL,
};
use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

use crate::data_model::event::trace::{
    AttributeValue, AttributeValueExt as _, Span, SpanEvent, SpanLink, Trace, TraceIngestInfo,
};

fn string(s: &'static str) -> AttributeValue {
    AttributeValue::String(MetaString::from_static(s))
}

fn attributes(entries: &[(&'static str, AttributeValue)]) -> FastHashMap<MetaString, AttributeValue> {
    entries
        .iter()
        .map(|(k, v)| (MetaString::from_static(k), v.clone()))
        .collect()
}

#[test]
fn attribute_value_ext() {
    assert_eq!(string("a").as_string(), Some(&MetaString::from_static("a")));
    assert_eq!(AttributeValue::Int(1).as_string(), None);
    assert_eq!(AttributeValue::Float(1.5).as_float(), Some(1.5));
    assert_eq!(AttributeValue::Int(1).as_float(), None);
    assert_eq!(AttributeValue::Bool(true).as_bool(), Some(true));
    assert_eq!(AttributeValue::Int(1).as_bool(), None);
    assert_eq!(AttributeValue::Int(7).as_int(), Some(7));
    assert_eq!(AttributeValue::Float(7.0).as_int(), None);
    assert_eq!(AttributeValue::Bytes(b"ab".to_vec()).as_bytes(), Some(&b"ab"[..]));
    assert_eq!(string("ab").as_bytes(), None);
    // The `libdd-trace-model` accessors.
    assert_eq!(string("a").as_str(), Some("a"));
    assert_eq!(AttributeValue::Int(2).as_f64(), Some(2.0));
    assert_eq!(AttributeValue::Float(2.0).as_f64(), Some(2.0));
}

#[test]
fn key_value_list_keeps_order_and_hides_shadowed_entries() {
    let mut map: Vec<(MetaString, AttributeValue)> = vec![
        (MetaString::from_static("a"), AttributeValue::Int(1)),
        (MetaString::from_static("b"), AttributeValue::Int(2)),
        (MetaString::from_static("a"), AttributeValue::Int(3)),
    ];
    assert_eq!(ValueMap::get(&map, "a"), Some(&AttributeValue::Int(3)));
    assert!(ValueMap::contains_key(&map, "b"));
    assert!(!ValueMap::contains_key(&map, "c"));
    assert_eq!(ValueMap::len(&map), 2);
    let entries: Vec<_> = ValueMap::iter(&map).collect();
    assert_eq!(
        entries,
        [("b", &AttributeValue::Int(2)), ("a", &AttributeValue::Int(3))]
    );
    assert_eq!(
        AttributeValue::KeyValueList(map.clone()).format_as_string(),
        "{b=2,a=3}"
    );

    *ValueMapMut::get_mut(&mut map, "a").unwrap() = AttributeValue::Int(4);
    assert_eq!(ValueMap::get(&map, "a"), Some(&AttributeValue::Int(4)));

    assert_eq!(
        ValueMapMut::insert(&mut map, MetaString::from_static("a"), AttributeValue::Int(5)),
        Some(AttributeValue::Int(4))
    );
    assert_eq!(
        ValueMapMut::insert(&mut map, MetaString::from_static("c"), AttributeValue::Int(6)),
        None
    );
    assert_eq!(ValueMap::len(&map), 3);

    // Mutable traversals drop the shadowed entries first.
    let keys: Vec<_> = ValueMapMut::iter_mut(&mut map).map(|(k, _)| k.to_owned()).collect();
    assert_eq!(keys, ["b", "a", "c"]);
    assert_eq!(map.len(), 3);

    ValueMapMut::retain(&mut map, |k, v| {
        if let AttributeValue::Int(i) = v {
            *i += 10;
        }
        k != "b"
    });
    assert_eq!(
        map,
        [
            (MetaString::from_static("a"), AttributeValue::Int(15)),
            (MetaString::from_static("c"), AttributeValue::Int(16)),
        ]
    );

    // Removal returns the value that lookups return, and drops the shadowed entries too.
    map.push((MetaString::from_static("c"), AttributeValue::Int(17)));
    assert_eq!(ValueMapMut::remove(&mut map, "c"), Some(AttributeValue::Int(17)));
    assert_eq!(ValueMapMut::remove(&mut map, "c"), None);
    assert_eq!(map, [(MetaString::from_static("a"), AttributeValue::Int(15))]);
}

fn test_span() -> Span {
    Span::new("svc", "op", "res", "web", 2, 1, 1_700_000_000_000_000_000, 5_000, 1)
        .with_env("prod")
        .with_version("1.2")
        .with_component("http")
        .with_kind(2)
        .with_attributes(attributes(&[
            ("s", string("v")),
            ("i", AttributeValue::Int(3)),
            ("f", AttributeValue::Float(2.5)),
            ("b", AttributeValue::Bool(true)),
        ]))
        .with_span_links(vec![SpanLink::new(7, 8)])
        .with_span_events(vec![SpanEvent::new(9, "event")])
}

#[test]
fn span_read_accessors() {
    let span = test_span();
    assert_eq!(span.service(), "svc");
    assert_eq!(span.name(), "op");
    assert_eq!(span.resource(), "res");
    assert_eq!(span.typ(), "web");
    assert_eq!(libdd_trace_model::Span::span_id(&span), 2);
    assert_eq!(span.parent_id(), 1);
    assert_eq!(libdd_trace_model::Span::start(&span), 1_700_000_000_000_000_000);
    assert_eq!(libdd_trace_model::Span::duration(&span), 5_000);
    assert!(span.is_error());
    assert_eq!(span.span_kind(), Some("server"));
    assert_eq!(span.env(), Some("prod"));
    assert_eq!(span.version(), Some("1.2"));
    assert_eq!(span.component(), Some("http"));
    assert_eq!(libdd_trace_model::Span::span_links(&span).count(), 1);
    assert_eq!(libdd_trace_model::Span::span_events(&span).count(), 1);

    let span = Span::default();
    assert!(!span.is_error());
    assert_eq!(
        (span.span_kind(), span.env(), span.version(), span.component()),
        (None, None, None, None)
    );
}

#[test]
fn span_setters_round_trip() {
    let mut span = Span::default();
    SpanMut::set_service(&mut span, MetaString::from_static("svc"));
    SpanMut::set_resource(&mut span, MetaString::from_static("res"));
    SpanMut::set_name(&mut span, MetaString::from_static("op"));
    SpanMut::set_type(&mut span, MetaString::from_static("db"));
    SpanMut::set_parent_id(&mut span, 4);
    SpanMut::set_start(&mut span, 1_700_000_000_000_000_000);
    SpanMut::set_duration(&mut span, 10);
    SpanMut::set_error(&mut span, true);
    SpanMut::set_span_kind(&mut span, "client");
    SpanMut::set_env(&mut span, MetaString::from_static("prod"));
    SpanMut::set_version(&mut span, MetaString::from_static("1.2"));
    SpanMut::set_component(&mut span, MetaString::from_static("grpc"));

    assert_eq!(
        (
            Span::service(&span),
            Span::resource(&span),
            Span::name(&span),
            span.span_type()
        ),
        ("svc", "res", "op", "db")
    );
    assert_eq!(Span::parent_id(&span), 4);
    assert_eq!(
        (Span::start(&span), Span::duration(&span)),
        (1_700_000_000_000_000_000, 10)
    );
    assert_eq!(span.error(), 1);
    assert_eq!(span.kind, 3);
    assert_eq!(
        (span.env.as_ref(), span.version.as_ref(), span.component.as_ref()),
        ("prod", "1.2", "grpc")
    );

    SpanMut::set_error(&mut span, false);
    assert_eq!(span.error(), 0);
    // An empty value unsets the promoted tag.
    SpanMut::set_env(&mut span, MetaString::empty());
    SpanMut::set_version(&mut span, MetaString::empty());
    SpanMut::set_component(&mut span, MetaString::empty());
    assert_eq!((span.env(), span.version(), span.component()), (None, None, None));
}

#[test]
fn span_start_and_duration_saturate() {
    let mut span = Span::default().with_start(u64::MAX).with_duration(i64::MAX as u64 + 1);
    assert_eq!(libdd_trace_model::Span::start(&span), i64::MAX);
    assert_eq!(libdd_trace_model::Span::duration(&span), i64::MAX);

    SpanMut::set_start(&mut span, i64::MAX);
    SpanMut::set_duration(&mut span, -1);
    assert_eq!((Span::start(&span), Span::duration(&span)), (i64::MAX as u64, 0));
    SpanMut::set_start(&mut span, i64::MIN);
    assert_eq!(Span::start(&span), 0);
}

#[test]
fn span_kind_maps_to_otel_values() {
    for (kind, name) in [
        (1, "internal"),
        (2, "server"),
        (3, "client"),
        (4, "producer"),
        (5, "consumer"),
    ] {
        let mut span = Span::default().with_kind(kind);
        assert_eq!(span.span_kind(), Some(name));
        span.set_span_kind("");
        assert_eq!(span.kind, 0);
        span.set_span_kind(name);
        assert_eq!(span.kind, kind);
    }
    // Unset and unknown kinds have no name, and an unknown name unsets the kind.
    assert_eq!(Span::default().with_kind(0).span_kind(), None);
    assert_eq!(Span::default().with_kind(6).span_kind(), None);
    let mut span = Span::default().with_kind(2);
    span.set_span_kind("Server");
    assert_eq!(span.kind, 0);
}

#[test]
fn span_attributes_round_trip() {
    let mut span = test_span();
    assert_eq!(span.attribute_str("s"), Some("v"));
    assert_eq!(span.attribute_str("i"), None);
    assert_eq!(span.attribute_f64("i"), Some(3.0));
    assert_eq!(span.attribute_f64("f"), Some(2.5));
    assert_eq!(span.attribute_i64("i"), Some(3));
    assert_eq!(span.attribute_i64("f"), None);
    assert!(span.has_attribute("b"));
    assert!(!span.has_attribute("missing"));
    assert_eq!(span.attribute("s"), Some(AttributeRef::String("v")));
    assert_eq!(span.attribute("b"), Some(AttributeRef::Bool(true)));
    assert_eq!(span.attribute("f"), Some(AttributeRef::Float(2.5)));
    assert_eq!(span.attribute("missing"), None);
    let mut keys: Vec<_> = libdd_trace_model::AttributesIter::attributes(&span)
        .map(|(k, _)| k)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["b", "f", "i", "s"]);

    span.set_attribute(MetaString::from_static("s"), string("w"));
    assert_eq!(span.attribute_str("s"), Some("w"));
    *span.attribute_mut("i").unwrap() = AttributeValue::Int(4);
    assert_eq!(span.attributes.get("i"), Some(&AttributeValue::Int(4)));
    assert_eq!(span.remove_attribute("b"), Some(AttributeValue::Bool(true)));
    assert_eq!(span.remove_attribute("b"), None);
    span.retain_attributes(|k, _| k != "f");
    assert!(!span.has_attribute("f"));
    assert_eq!(span.attributes.len(), 2);
}

/// The attribute accessors reach only the attribute map, not the fields of the promoted tags.
#[test]
fn span_attributes_skip_promoted_fields() {
    let mut span = test_span();
    assert_eq!(span.attribute_str("env"), None);
    assert_eq!(span.compat().attribute_str("env"), Some("prod"));

    span.set_attribute(MetaString::from_static("env"), string("staging"));
    assert_eq!(span.env(), Some("prod"));
    assert_eq!(span.remove_attribute("env"), Some(string("staging")));
    assert_eq!(span.env(), Some("prod"));

    span.compat_mut()
        .set_attribute(MetaString::from_static("span.kind"), string("consumer"));
    assert_eq!(span.kind, 5);
    assert!(!span.has_attribute("span.kind"));
}

#[test]
fn span_link_accessors() {
    let mut link = SpanLink::new(0x0102, 3)
        .with_trace_id_high(0x0a0b)
        .with_tracestate("dd=s:1")
        .with_flags(SPAN_LINK_FLAGS_SET_SENTINEL | 1)
        .with_attributes(attributes(&[("link.name", string("next"))]));
    assert_eq!(libdd_trace_model::SpanLink::trace_id(&link), (0x0a0b << 64) | 0x0102);
    assert_eq!(libdd_trace_model::SpanLink::span_id(&link), 3);
    assert_eq!(libdd_trace_model::SpanLink::tracestate(&link), "dd=s:1");
    assert_eq!(libdd_trace_model::SpanLink::flags(&link), 1);
    assert_eq!(link.attribute_str("link.name"), Some("next"));
    assert_eq!(link.attribute("link.name"), Some(AttributeRef::String("next")));

    *link.attribute_mut("link.name").unwrap() = string("other");
    link.set_attribute(MetaString::from_static("k"), AttributeValue::Int(1));
    assert_eq!(link.attribute_str("link.name"), Some("other"));
    assert_eq!(link.attribute_i64("k"), Some(1));
    assert_eq!(link.remove_attribute("k"), Some(AttributeValue::Int(1)));
    link.retain_attributes(|_, _| false);
    assert!(SpanLink::attributes(&link).is_empty());
}

#[test]
fn span_event_accessors() {
    let mut event = SpanEvent::new(9, "event").with_attributes(attributes(&[("k", AttributeValue::Float(1.0))]));
    assert_eq!(libdd_trace_model::SpanEvent::name(&event), "event");
    assert_eq!(libdd_trace_model::SpanEvent::time_unix_nano(&event), 9);
    assert_eq!(event.attribute_i64("k"), Some(1));
    assert_eq!(event.attribute("k"), Some(AttributeRef::Float(1.0)));

    event.set_attribute(MetaString::from_static("s"), string("v"));
    assert_eq!(event.attribute_str("s"), Some("v"));
    *event.attribute_mut("s").unwrap() = string("w");
    assert_eq!(event.remove_attribute("s"), Some(string("w")));
    event.retain_attributes(|_, _| false);
    assert!(SpanEvent::attributes(&event).is_empty());
}

#[test]
fn span_mut_reaches_links_and_events() {
    let mut span = test_span();
    for link in span.span_links_mut() {
        link.set_attribute(MetaString::from_static("k"), AttributeValue::Bool(true));
    }
    for event in span.span_events_mut() {
        event.set_attribute(MetaString::from_static("k"), AttributeValue::Int(1));
    }
    assert_eq!(Span::span_links(&span)[0].attribute_str("k"), None);
    assert!(Span::span_links(&span)[0].has_attribute("k"));
    assert_eq!(Span::span_events(&span)[0].attribute_i64("k"), Some(1));
}

fn test_trace() -> Trace {
    let mut trace = Trace::new(vec![test_span(), test_span().with_span_id(3)], TraceIngestInfo::Datadog);
    trace.trace_id_high = 0x0102;
    trace.trace_id_low = 0x0304;
    trace.origin = MetaString::from_static("lambda");
    trace.attributes = Arc::new(attributes(&[("_dd.p.tid", string("abc"))]));
    trace
}

#[test]
fn trace_chunk_accessors_round_trip() {
    let mut trace = test_trace();
    assert_eq!(libdd_trace_model::TraceChunk::trace_id(&trace), (0x0102 << 64) | 0x0304);
    assert_eq!(trace.origin(), "lambda");
    assert_eq!(trace.priority(), None);
    assert_eq!(trace.sampling_mechanism(), None);
    assert!(!trace.dropped_trace());
    assert_eq!(libdd_trace_model::TraceChunk::spans(&trace).count(), 2);
    assert_eq!(trace.attribute_str("_dd.p.tid"), Some("abc"));
    assert_eq!(trace.attribute("_dd.p.tid"), Some(AttributeRef::String("abc")));

    trace.set_origin(MetaString::from_static("rum"));
    trace.set_priority(2);
    trace.set_sampling_mechanism(4);
    trace.set_dropped_trace(true);
    assert_eq!(
        (
            trace.origin.as_ref(),
            trace.priority,
            trace.sampling_mechanism,
            trace.dropped_trace
        ),
        ("rum", Some(2), 4, true)
    );
    assert_eq!(trace.priority(), Some(2));
    assert_eq!(trace.sampling_mechanism(), Some(4));
    assert!(trace.dropped_trace());

    for span in libdd_trace_model::TraceChunkMut::spans_mut(&mut trace) {
        span.set_name(MetaString::from_static("renamed"));
    }
    assert!(Trace::spans(&trace).iter().all(|span| Span::name(span) == "renamed"));
    libdd_trace_model::TraceChunkMut::retain_spans(&mut trace, |span| Span::span_id(span) == 3);
    assert_eq!(Trace::spans(&trace).len(), 1);
}

#[test]
fn trace_attributes_clone_only_on_change() {
    let mut trace = test_trace();
    let other = trace.clone();
    assert!(Arc::ptr_eq(&trace.attributes, &other.attributes));

    // Reads, and writes that don't find the key, keep the shared map.
    assert!(trace.attribute_mut("missing").is_none());
    assert_eq!(trace.remove_attribute("missing"), None);
    assert!(Arc::ptr_eq(&trace.attributes, &other.attributes));

    trace.set_attribute(MetaString::from_static("k"), AttributeValue::Int(1));
    assert!(!Arc::ptr_eq(&trace.attributes, &other.attributes));
    assert_eq!(trace.attribute_i64("k"), Some(1));
    assert!(!other.has_attribute("k"));

    *trace.attribute_mut("k").unwrap() = AttributeValue::Int(2);
    assert_eq!(trace.attribute_i64("k"), Some(2));
    assert_eq!(trace.remove_attribute("k"), Some(AttributeValue::Int(2)));
    trace.retain_attributes(|_, _| false);
    assert!(trace.attributes.is_empty());
    assert_eq!(other.attribute_str("_dd.p.tid"), Some("abc"));
}
