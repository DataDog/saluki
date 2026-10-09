use std::time::{Duration, Instant};

use proptest::{collection::vec, prelude::*};
use saluki_core::{
    components::transforms::SynchronousTransform as _,
    data_model::{
        event::metric::{context::Context, Metric},
        tags::{SharedTagSet, Tag, TagSet},
    },
    execution::{AdmissionTime, BatchHeader},
};
use stringtheory::MetaString;

use super::{reference::HostTagsEnrichment, HostEnrichment, HostTags};
use crate::{
    test_support::{buffer_of, consume, describe, metrics_of},
    testing::{arb_metric, arb_tags},
};

fn shared(tags: &[&str]) -> SharedTagSet {
    tags.iter().map(|tag| Tag::from(*tag)).collect()
}

fn tags_of(metric: &Metric) -> Vec<String> {
    metric
        .context()
        .tags()
        .into_iter()
        .map(|tag| tag.as_str().to_string())
        .collect()
}

#[test]
fn host_tags_are_appended_as_they_are() {
    let host_tags = HostTags::new(
        shared(&["zone:a", "cloud_provider:example"]),
        AdmissionTime::from_unix_secs(1000),
    );
    let context = Context::from_static_parts("app.requests", &["zone:a", "env:prod"])
        .with_origin_tags(["origin:x"].into_iter().map(Tag::from).collect::<TagSet>())
        .with_host(MetaString::from_static("web-1"));
    let mut metric = Metric::gauge(context.clone(), 1.0);

    host_tags.apply(&mut metric);
    // The metric's own `zone:a` stays, and the host's is added too: values are not deduplicated. (Where the host's
    // tags land in the tag order depends on how the metric's tag set was built; the property test pins it to native.)
    let expected = ["cloud_provider:example", "env:prod", "zone:a", "zone:a"];
    assert_eq!(sorted(tags_of(&metric)), expected);
    // Only the instrumented tags change.
    assert_eq!(metric.context().name(), "app.requests");
    assert_eq!(metric.context().host(), Some("web-1"));
    assert_eq!(metric.context().origin_tags(), context.origin_tags());
    assert_eq!(metric.values(), Metric::gauge(context.clone(), 1.0).values());

    // Adding the same host tags again adds nothing.
    host_tags.apply(&mut metric);
    assert_eq!(sorted(tags_of(&metric)), expected);
}

fn sorted(mut tags: Vec<String>) -> Vec<String> {
    tags.sort();
    tags
}

#[test]
fn the_kernel_adds_host_tags_to_batches_admitted_before_the_deadline() {
    let mut kernel = HostTags::new(shared(&["zone:a"]), AdmissionTime::from_unix_secs(1000));
    // (admission time, whether the batch gains the host tags)
    let cases = [
        (Some(AdmissionTime::UNIX_EPOCH), true),
        (Some(AdmissionTime::from_unix_nanos(999_999_999_999)), true),
        (Some(AdmissionTime::from_unix_secs(1000)), false),
        (Some(AdmissionTime::from_unix_secs(1001)), false),
        (None, false),
    ];
    for (admitted_at, applies) in cases {
        let header = BatchHeader {
            admitted_at,
            ..Default::default()
        };
        assert_eq!(kernel.applies_to(admitted_at), applies);
        let consumed = consume(
            &mut kernel,
            &header,
            1,
            buffer_of([Metric::counter("a", 1.0), Metric::gauge("b", 2.0)]),
        );
        let expected: &[&str] = if applies { &["zone:a"] } else { &[] };
        for metric in &consumed.ports[0] {
            assert_eq!(tags_of(metric), expected, "{:?}", admitted_at);
        }
        assert_eq!(consumed.buffers, [1]);
    }
}

#[test]
fn host_enrichment_leaves_metrics_unchanged() {
    let mut kernel = HostEnrichment;
    let metrics = [
        Metric::gauge(Context::from_static_name("unset"), 1.0),
        Metric::gauge(
            Context::from_static_name("empty").with_host(MetaString::from_static("")),
            1.0,
        ),
        Metric::gauge(
            Context::from_static_name("custom").with_host(MetaString::from_static("custom-host")),
            1.0,
        ),
    ];
    let consumed = consume(&mut kernel, &BatchHeader::default(), 1, buffer_of(metrics.clone()));
    assert_eq!(consumed.ports[0], metrics);
    assert_eq!(consumed.buffers, [1]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_host_tags_match_the_pre_extraction_transform(
        host_tags in arb_tags(),
        metrics in vec(arb_metric(), 0..24),
    ) {
        let host_tags = host_tags.into_iter().map(Tag::from).collect::<SharedTagSet>();
        let deadline = AdmissionTime::from_unix_secs(1000);
        let mut kernel = HostTags::new(host_tags.clone(), deadline);

        // Before the deadline, the tags are added exactly as natively.
        let mut native = HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: Duration::from_secs(3600),
            host_tags: Some(host_tags.clone()),
        };
        let mut expected = buffer_of(metrics.iter().cloned());
        native.transform_buffer(&mut expected);
        let before = BatchHeader { admitted_at: Some(AdmissionTime::from_unix_secs(999)), ..Default::default() };
        let consumed = consume(&mut kernel, &before, 1, buffer_of(metrics.iter().cloned()));
        let expected = metrics_of(expected);
        prop_assert_eq!(expected.len(), consumed.ports[0].len());
        for (expected, actual) in expected.iter().zip(&consumed.ports[0]) {
            prop_assert_eq!(describe(expected), describe(actual));
            prop_assert_eq!(expected.context(), actual.context());
        }

        // After it, nothing changes, as natively once the duration has elapsed.
        let mut native = HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: Duration::ZERO,
            host_tags: Some(host_tags),
        };
        let mut expected = buffer_of(metrics.iter().cloned());
        native.transform_buffer(&mut expected);
        let after = BatchHeader { admitted_at: Some(deadline), ..Default::default() };
        let consumed = consume(&mut kernel, &after, 1, buffer_of(metrics.iter().cloned()));
        let expected = metrics_of(expected);
        prop_assert_eq!(
            expected.iter().map(describe).collect::<Vec<_>>(),
            consumed.ports[0].iter().map(describe).collect::<Vec<_>>()
        );
        prop_assert_eq!(
            metrics.iter().map(describe).collect::<Vec<_>>(),
            consumed.ports[0].iter().map(describe).collect::<Vec<_>>()
        );
    }
}
