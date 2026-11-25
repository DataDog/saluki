//! Allocation test for the Datadog trace normalizer.
//!
//! The normalizer works on a `Trace` in place, through the `libdd-trace-model` traits. This test checks that it makes
//! no heap allocation for a trace that is already normalized: the reads borrow the `MetaString`s, and nothing is
//! written back. It also checks that a repair that copies a field to another one allocates once for each span.
//!
//! The global allocator must track every allocation, so this is an integration test without the libtest harness. See
//! `lib/process-memory/tests/no_runtime_allocations.rs`.

#[cfg(not(target_os = "aix"))]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(not(target_os = "aix"))]
fn normalized_trace() -> saluki_core::data_model::event::trace::Trace {
    use saluki_common::collections::FastHashMap;
    use saluki_core::data_model::event::trace::{AttributeValue, Span, SpanLink, Trace, TraceIngestInfo};
    use stringtheory::MetaString;

    // Owned strings, as a decoder makes them, so that a copy can't hide behind a cheap clone.
    let text = |s: &str| MetaString::from(s.to_owned());
    let attributes = |entries: &[(&str, AttributeValue)]| -> FastHashMap<MetaString, AttributeValue> {
        entries.iter().map(|(k, v)| (text(k), v.clone())).collect()
    };
    let span = |span_id| {
        Span::new(
            text("django"),
            text("django.controller"),
            text("GET /some/raclette"),
            text("http"),
            span_id,
            1111,
            1_448_466_874_000_000_000,
            10_000_000,
            0,
        )
        .with_env(text("prod"))
        .with_component(text("django"))
        .with_attributes(attributes(&[
            ("peer.service", AttributeValue::String(text("postgres"))),
            ("_dd.base_service", AttributeValue::String(text("django-app"))),
            ("http.status_code", AttributeValue::String(text("200"))),
            ("user", AttributeValue::String(text("leo"))),
            ("cheese_weight", AttributeValue::Float(100000.0)),
        ]))
        .with_span_links(vec![SpanLink::new(42, 1).with_attributes(attributes(&[(
            "link.name",
            AttributeValue::String(text("next_request")),
        )]))])
    };

    let mut trace = Trace::new(vec![span(1388), span(1389), span(1388)], TraceIngestInfo::Datadog);
    trace.trace_id_low = 424242;
    trace.payload.language_name = text("python");
    trace
}

fn main() {
    #[cfg(not(target_os = "aix"))]
    {
        use libdd_trace_normalization::normalizer_v1::{
            count_duplicate_span_ids, normalize_chunk_with, MalformedReason, NormalizeObserver, NormalizeOptions,
        };
        use saluki_common::collections::FastHashSet;
        use saluki_core::data_model::event::trace::Trace;
        use stringtheory::MetaString;

        /// Counts the repairs without allocating.
        #[derive(Default)]
        struct Counter(usize);

        impl NormalizeObserver for Counter {
            fn malformed(&mut self, _: MalformedReason) {
                self.0 += 1;
            }
        }

        let mut seen_span_ids = FastHashSet::default();
        seen_span_ids.reserve(16);
        let profiler = dhat::Profiler::builder().testing().build();

        // Normalizes `trace` and returns the number of repairs and the number of heap blocks allocated. The stats are
        // cumulative, so each case reads them before and after.
        let mut normalize = |trace: &mut Trace, component2name: bool| {
            let lang = trace.payload.language_name.clone();
            let opts = NormalizeOptions {
                lang: &lang,
                component2name,
            };
            let mut counter = Counter::default();
            let before = dhat::HeapStats::get().total_blocks;
            let result = normalize_chunk_with(trace, &opts, &mut counter);
            count_duplicate_span_ids(trace, &mut seen_span_ids, &mut counter);
            let blocks = dhat::HeapStats::get().total_blocks - before;
            assert_eq!(result, Ok(()));
            (counter.0, blocks)
        };

        // A trace that is already normalized. The only repair is the duplicate span ID, which changes nothing.
        let mut trace = normalized_trace();
        let expected = trace.clone();
        let normalized = normalize(&mut trace, false);
        assert_eq!(trace, expected);

        // An empty resource gets a copy of the name: one allocation for each of the 3 spans.
        let mut trace = normalized_trace();
        for span in trace.spans_mut() {
            *span = span.clone().with_resource(MetaString::empty());
        }
        let empty_resource = normalize(&mut trace, false);
        assert!(trace.spans().iter().all(|span| span.resource() == "django.controller"));

        // With `component2name`, a copy of the component replaces the name: one allocation for each of the 3 spans.
        let mut trace = normalized_trace();
        let component2name = normalize(&mut trace, true);
        assert!(trace.spans().iter().all(|span| span.name() == "django"));

        drop(profiler);
        assert_eq!(normalized, (1, 0), "already normalized: (repairs, heap blocks)");
        assert_eq!(empty_resource, (4, 3), "empty resource: (repairs, heap blocks)");
        assert_eq!(component2name, (1, 3), "component2name: (repairs, heap blocks)");
    }
}
