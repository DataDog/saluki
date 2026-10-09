use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::data_model::{
    event::{
        metric::{context::Context, Metric, MetricValues},
        Event,
    },
    tags::TagSet,
};

use super::{
    histogram_statistic_entries, is_histogram_statistic_entry, is_scalar_series, reference, IntakeNameAllowlist,
    RawNameSet,
};
use crate::{
    matcher::{compact_prefixes, SortedSetMatcher},
    params::{HistogramAggregate, HistogramParams},
    testing::{arb_metric, arb_metric_name, NAME_VOCABULARY, PREFIX_VOCABULARY},
};

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn scalar_series_are_counters_rates_gauges_and_sets() {
    let cases = [
        (MetricValues::counter(1.0), true),
        (MetricValues::rate(1.0, std::time::Duration::from_secs(10)), true),
        (MetricValues::gauge(1.0), true),
        (MetricValues::set("member".to_string()), true),
        (MetricValues::histogram(1.0), false),
        (MetricValues::distribution(1.0), false),
    ];
    for (values, expected) in cases {
        assert_eq!(is_scalar_series(&values), expected, "{:?}", values);
    }
}

#[test]
fn raw_name_sets_compare_names_verbatim() {
    let set = RawNameSet::new(["my-service.requests", "allowed", "allowed"]);
    assert_eq!(set.len(), 2);
    assert!(set.contains("my-service.requests"));
    assert!(set.contains("allowed"));
    // No normalization, and no prefix matching.
    assert!(!set.contains("my_service.requests"));
    assert!(!set.contains("allowed.extra"));
    assert!(!set.contains("allow"));

    let empty = RawNameSet::new(Vec::<String>::new());
    assert!(empty.is_empty());
    assert!(!empty.contains(""));
}

#[test]
fn intake_name_allowlists_combine_exact_names_and_literal_prefixes() {
    let allowlist = IntakeNameAllowlist::new(
        &["exact.metric", "my_service.requests", "literal-name", "exact.metric"],
        &[
            "billing.",
            "billing.latency.",
            "billing.",
            "literal.*",
            "redis.checkpoint_",
        ],
    );
    assert_eq!(allowlist.name_count(), 3);
    assert_eq!(allowlist.prefix_count(), 3);
    // (name, expected)
    let cases = [
        ("exact.metric", true),
        ("billing.", true),
        ("billing.latency.p99", true),
        ("billing.requests", true),
        // Names are normalized as the intake stores them before they are compared.
        ("my-service.requests", true),
        ("my_service.requests", true),
        ("redis.checkpoint-bytes", true),
        // Entries are not: an entry that is not a normalized name matches nothing exactly.
        ("literal-name", false),
        ("literal.name", false),
        ("literal.*name", false),
        ("exact.metric.extra", false),
        ("billing", false),
        ("billing_other", false),
        ("Billing.requests", false),
        ("redis.checkpointing.count", false),
        ("other", false),
        // A name with no intake name never matches.
        ("123", false),
        ("", false),
    ];
    for (name, expected) in cases {
        assert_eq!(allowlist.matches(name), expected, "{}", name);
    }

    // The empty prefix matches every name that has an intake name; nothing matches an empty allowlist.
    let everything = IntakeNameAllowlist::new(&[] as &[&str], &[""]);
    assert!(everything.matches("any.metric"));
    assert!(!everything.matches("123"));
    let nothing = IntakeNameAllowlist::new(&[] as &[&str], &[] as &[&str]);
    assert!(!nothing.matches("any.metric"));
}

#[test]
fn histogram_statistic_entries_name_a_statistics_series() {
    // Ported from the native post-aggregate filter's test of its histogram statistic filterlist derivation.
    let histogram = HistogramParams::from_names(&["avg", "max", "median"], &["0.73", "0.22"], false, String::new())
        .expect("valid statistics");
    let values = strings(&[
        "foo",
        "bar",
        "baz",
        "foomax",
        "foo.avg",
        "foo.max",
        "foo.count",
        "baz.73percentile",
        "bar.50percentile",
        "bar.22percentile",
        "count",
        ".max",
        "max",
    ]);
    assert_eq!(
        histogram_statistic_entries(&values, &histogram),
        ["foo.avg", "foo.max", "baz.73percentile", "bar.22percentile", ".max"]
    );

    assert!(is_histogram_statistic_entry("a.b.95percentile", &["95percentile"]));
    assert!(!is_histogram_statistic_entry("a.b95percentile", &["95percentile"]));
    assert!(!is_histogram_statistic_entry("a.max", &[] as &[&str]));
}

fn event(metric: &Metric) -> Event {
    Event::Metric(metric.clone())
}

/// Allowlist entries: names metrics have, normalized or not, and prefixes of them.
fn arb_entries() -> impl Strategy<Value = Vec<String>> {
    let entry = prop_oneof![
        select(NAME_VOCABULARY).prop_map(str::to_string),
        select(PREFIX_VOCABULARY).prop_map(str::to_string),
        "[a-z_.]{0,6}",
    ];
    vec(entry, 0..5)
}

fn arb_aggregates() -> impl Strategy<Value = Vec<&'static str>> {
    vec(
        select(HistogramAggregate::ALL.map(HistogramAggregate::as_str).to_vec()),
        0..4,
    )
}

fn arb_quantiles() -> impl Strategy<Value = Vec<&'static str>> {
    vec(
        select(vec!["0.95", "0.5", "0", "1", "0.955", "0.004", "0.005", "0.299"]),
        0..3,
    )
}

/// Filterlist entries that name statistics' series of names metrics have, and other entries.
fn arb_filterlist() -> impl Strategy<Value = Vec<String>> {
    let suffix = select(vec![
        "max",
        "median",
        "avg",
        "count",
        "sum",
        "min",
        "95percentile",
        "96percentile",
        "50percentile",
        "0percentile",
        "100percentile",
        "1percentile",
        "30percentile",
    ]);
    let entry = prop_oneof![
        (select(NAME_VOCABULARY), suffix.clone()).prop_map(|(name, suffix)| format!("{}.{}", name, suffix)),
        (select(NAME_VOCABULARY), suffix).prop_map(|(name, suffix)| format!("{}{}", name, suffix)),
        select(NAME_VOCABULARY).prop_map(str::to_string),
    ];
    vec(entry, 0..5)
}

/// Metrics named like the series of a histogram statistic, as aggregation emits them, and other metrics.
fn arb_series_metric() -> impl Strategy<Value = Metric> {
    (arb_metric(), arb_filterlist(), any::<prop::sample::Index>()).prop_map(|(metric, names, index)| {
        if names.is_empty() {
            return metric;
        }
        let name = names[index.index(names.len())].clone();
        let (context, values, metadata) = metric.into_parts();
        Metric::from_parts(context.with_name(name), values, metadata)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_prefix_compaction_matches_the_pre_extraction_compaction(
        prefixes in vec(prop_oneof![select(PREFIX_VOCABULARY).prop_map(str::to_string), "[ab.]{0,4}"], 0..8),
    ) {
        let mut expected = prefixes.clone();
        reference::compact_metric_prefixes(&mut expected);
        let mut actual = prefixes.clone();
        compact_prefixes(&mut actual);
        prop_assert_eq!(expected, actual);
    }

    #[test]
    fn property_test_endpoint_allowlists_match_the_pre_extraction_filter(
        names in arb_entries(),
        prefixes in arb_entries(),
        metric_names in vec(arb_metric_name(), 1..16),
    ) {
        let expected = reference::Filter::for_allowlist(&names, &prefixes);
        let actual = IntakeNameAllowlist::new(&names, &prefixes);
        prop_assert_eq!(
            reference::allowlist_sizes(&expected),
            Some((actual.name_count(), actual.prefix_count()))
        );
        for name in metric_names {
            let metric = Metric::counter(Context::from_parts(name.clone(), TagSet::default()), 1.0);
            prop_assert_eq!(expected.should_forward(&event(&metric)), actual.matches(&name), "{:?}", name);
        }
    }

    #[test]
    fn property_test_mirroring_and_autoscaling_selection_match_the_pre_extraction_gateways(
        enabled in any::<bool>(),
        mirroring in any::<bool>(),
        allowlist in arb_entries(),
        metrics in vec(arb_metric(), 1..16),
    ) {
        // Multi-region failover: nothing unless both flags are on; then everything with an empty allowlist, and the
        // raw names on it otherwise.
        let mrf = reference::Filter::for_mrf(
            enabled,
            &reference::MetricMirroring { enabled: mirroring, allowlist: allowlist.clone() },
        );
        let names = RawNameSet::new(&allowlist);
        for metric in &metrics {
            let name = metric.context().name().as_ref();
            let actual = enabled && mirroring && (names.is_empty() || names.contains(name));
            prop_assert_eq!(mrf.should_forward(&event(metric)), actual, "{:?}", name);
        }

        // Autoscaling failover: nothing unless enabled with a non-empty allowlist; then the scalar series whose raw
        // names are on it.
        let gateway = reference::AutoscalingFailoverGateway::new(reference::AutoscalingFailoverConfiguration {
            enabled,
            metrics: allowlist.clone(),
        });
        for metric in &metrics {
            let name = metric.context().name().as_ref();
            let actual = enabled && !names.is_empty() && is_scalar_series(metric.values()) && names.contains(name);
            prop_assert_eq!(gateway.should_forward(&event(metric)), actual, "{:?}", name);
        }
    }

    #[test]
    fn property_test_post_aggregation_filtering_matches_the_pre_extraction_filter(
        aggregates in arb_aggregates(),
        quantiles in arb_quantiles(),
        values in arb_filterlist(),
        match_prefix in any::<bool>(),
        metrics in vec(arb_series_metric(), 1..16),
    ) {
        let aggregate_names = aggregates.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let quantile_names = quantiles.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let mut expected = reference::DogStatsDPostAggregateFilter {
            matcher: SortedSetMatcher::default(),
            histogram_suffixes: reference::HistogramSuffixes::try_new(&aggregate_names, &quantile_names)
                .expect("generated statistics are valid"),
        };
        expected.sync_matcher(&values, match_prefix);
        let histogram = HistogramParams::from_names(&aggregates, &quantiles, false, String::new())
            .expect("generated statistics are valid");

        // The same entries, in the same order.
        let entries = histogram_statistic_entries(&values, &histogram);
        prop_assert_eq!(expected.histogram_values(&values), entries.clone());

        // The same metrics dropped, as a plan's `split` decides it: scalar series whose intake name matches.
        let matcher = SortedSetMatcher::new(&entries, match_prefix);
        for metric in &metrics {
            let actual = is_scalar_series(metric.values()) && matcher.matches_intake_name(metric.context().name());
            prop_assert_eq!(expected.should_filter_metric(metric), actual, "{:?}", metric.context().name());
        }
    }
}
