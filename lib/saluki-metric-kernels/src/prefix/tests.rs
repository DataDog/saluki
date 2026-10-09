use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::{
    data_model::{
        event::metric::{context::Context, Metric},
        tags::{Tag, TagSet},
    },
    execution::BatchHeader,
};
use stringtheory::MetaString;

use super::{reference::DogStatsDPrefixFilter, NamespacePrefix};
use crate::{
    matcher::SortedSetMatcher,
    params::NamespacePrefixParams,
    test_support::{buffer_of, consume, describe, metrics_of, multiset},
    testing::{arb_metric, NAME_VOCABULARY, PREFIX_VOCABULARY},
};

fn prefix(namespace: &str, exclusions: &[&str]) -> NamespacePrefix {
    NamespacePrefix::new(&NamespacePrefixParams {
        namespace: namespace.to_string(),
        exclusions: exclusions.iter().map(|exclusion| exclusion.to_string()).collect(),
    })
}

#[test]
fn names_are_prefixed_unless_excluded() {
    // (namespace, exclusions, name, name after prefixing)
    let cases: &[(&str, &[&str], &str, &str)] = &[
        // An empty namespace disables prefixing, whatever the exclusions.
        ("", &[], "app.requests", "app.requests"),
        ("", &["app."], "app.requests", "app.requests"),
        // A `.` is added to a namespace that does not end with one, and not to one that does.
        ("tenant", &[], "app.requests", "tenant.app.requests"),
        ("tenant.", &[], "app.requests", "tenant.app.requests"),
        ("tenant..", &[], "app", "tenant..app"),
        (".", &[], "app", ".app"),
        // Exclusions are compared verbatim against the raw name before prefixing, as plain prefixes.
        ("tenant", &["app."], "app.requests", "app.requests"),
        ("tenant", &["app."], "application", "tenant.application"),
        ("tenant", &["tenant."], "tenant.app", "tenant.app"),
        ("tenant", &["Tenant."], "tenant.app", "tenant.tenant.app"),
        ("tenant", &[""], "anything", "anything"),
        ("tenant", &["db", "app"], "application", "application"),
        // Names are prefixed as they are, even names that the intake would rewrite or reject.
        ("tenant", &[], "my metric-name", "tenant.my metric-name"),
        ("tenant", &[], "", "tenant."),
    ];
    for (namespace, exclusions, name, expected) in cases {
        let prefix = prefix(namespace, exclusions);
        let mut metric = Metric::gauge(Context::from_parts(name.to_string(), TagSet::default()), 1.0);
        prefix.apply(&mut metric);
        assert_eq!(
            metric.context().name(),
            expected,
            "namespace {:?}, exclusions {:?}, name {:?}",
            namespace,
            exclusions,
            name
        );
        assert_eq!(prefix.is_enabled(), !namespace.is_empty());
    }
}

#[test]
fn prefixed_names_are_new_strings_and_excluded_names_are_kept() {
    let prefix = prefix("tenant", &["internal."]);
    let name = MetaString::from("app.requests");
    assert_eq!(
        prefix.prefixed_name(&name),
        Some(MetaString::from("tenant.app.requests"))
    );

    let excluded = MetaString::from("internal.requests");
    assert_eq!(prefix.prefixed_name(&excluded), Some(excluded.clone()));

    let disabled = self::prefix("", &["internal."]);
    assert_eq!(disabled.prefixed_name(&name), None);
}

#[test]
fn prefixing_changes_only_the_name() {
    let prefix = prefix("tenant", &[]);
    let context = |name: &'static str| {
        Context::from_static_parts(name, &["env:prod", "bare"])
            .with_origin_tags(["origin:a"].into_iter().map(Tag::from).collect::<TagSet>())
            .with_host(MetaString::from_static("web-1"))
    };
    let mut metric = Metric::counter(context("app.requests"), 3.0);
    prefix.apply(&mut metric);

    let expected = context("tenant.app.requests");
    assert_eq!(describe(&metric), describe(&Metric::counter(expected, 3.0)));
}

#[test]
fn the_kernel_prefixes_its_batch_in_place() {
    let mut prefix = prefix("tenant", &["internal."]);
    let input = buffer_of([
        Metric::gauge("app.requests", 1.0),
        Metric::counter("internal.count", 2.0),
        Metric::distribution("latency", 3.0),
    ]);
    let consumed = consume(&mut prefix, &BatchHeader::default(), 1, input);

    let names = consumed.ports[0]
        .iter()
        .map(|metric| metric.context().name().to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["tenant.app.requests", "internal.count", "tenant.latency"]);
    // The input buffer leaves whole, and nothing is lost.
    assert_eq!(consumed.buffers, [1]);
    assert!(consumed.counters.is_empty());
}

/// Namespaces, including the empty one and ones that already end with a `.`.
fn arb_namespace() -> impl Strategy<Value = String> {
    prop_oneof![
        select(vec!["", "tenant", "tenant.", "app", "app.", "my", "."]).prop_map(str::to_string),
        select(PREFIX_VOCABULARY).prop_map(str::to_string),
    ]
}

/// Exclusions and filterlist entries drawn from the names and prefixes metrics have, with and without a namespace.
fn arb_entries() -> impl Strategy<Value = Vec<String>> {
    let entry = prop_oneof![
        select(NAME_VOCABULARY).prop_map(str::to_string),
        select(PREFIX_VOCABULARY).prop_map(str::to_string),
        (select(vec!["tenant.", "app.", "my", "."]), select(NAME_VOCABULARY))
            .prop_map(|(namespace, name)| format!("{}{}", namespace, name)),
    ];
    vec(entry, 0..4)
}

/// Applies the namespace kernel and then the listener filterlist the way a plan does: a `split` on the intake name of
/// the prefixed metric, whose matching side is dropped.
fn kernel_then_filter(prefix: &NamespacePrefix, filterlist: &SortedSetMatcher, mut metric: Metric) -> Option<Metric> {
    prefix.apply(&mut metric);
    (!filterlist.matches_intake_name(metric.context().name())).then_some(metric)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_prefixing_and_filtering_match_the_pre_extraction_transform(
        namespace in arb_namespace(),
        exclusions in arb_entries(),
        values in arb_entries(),
        match_prefix in any::<bool>(),
        metrics in vec(arb_metric(), 0..24),
    ) {
        let expected = DogStatsDPrefixFilter::new(&namespace, exclusions.clone(), &values, match_prefix);
        let prefix = NamespacePrefix::new(&NamespacePrefixParams { namespace: namespace.clone(), exclusions });
        let filterlist = SortedSetMatcher::new(&values, match_prefix);

        // One metric at a time: the same metrics are kept, with the same contexts.
        for metric in &metrics {
            let mut native = metric.clone();
            let kept = expected.process_metric(&mut native);
            let actual = kernel_then_filter(&prefix, &filterlist, metric.clone());
            prop_assert_eq!(kept, actual.is_some(), "metric {:?}", metric.context().name());
            if let Some(actual) = actual {
                prop_assert_eq!(describe(&native), describe(&actual));
                prop_assert_eq!(native.context(), actual.context());
            }
        }

        // A whole batch through the kernel, as a plan runs it: native filters reorder within a batch, so the
        // outputs compare as multisets.
        let mut native = buffer_of(metrics.iter().cloned());
        expected.transform_buffer(&mut native);
        let mut kernel = prefix.clone();
        let consumed = consume(&mut kernel, &BatchHeader::default(), 1, buffer_of(metrics.iter().cloned()));
        let kept = consumed.ports[0]
            .iter()
            .filter(|metric| !filterlist.matches_intake_name(metric.context().name()))
            .collect::<Vec<_>>();
        prop_assert_eq!(multiset(&metrics_of(native)), multiset(kept));
    }
}
