use metrics::set_default_local_recorder;
use proptest::{collection::vec, prelude::*, sample::select};
use saluki_core::{
    data_model::{
        event::metric::{
            context::{Context, TagSetMutViewState},
            Metric, MetricValues,
        },
        tags::{Tag, TagSet},
    },
    execution::BatchHeader,
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use super::{
    filter_metric_tags, is_projected, reference, ContextMemo, FilterMetricTagsOutcome, TagProjection,
    TagProjectionRules, TagProjectionTelemetry, ValueRules, NATIVE_CONTEXT_CACHE_ID,
};
use crate::{
    params::{InvalidTagValueRules, TagFilterAction, TagFilterRule, TagValueMismatch, TagValueRule},
    test_support::{buffer_of, consume, describe},
    testing::{arb_metric, NAME_VOCABULARY, PREFIX_VOCABULARY, TAG_KEY_VOCABULARY, TAG_VALUE_VOCABULARY},
};

fn distribution_metric(name: &'static str, tags: &[&'static str]) -> Metric {
    let context = Context::from_static_parts(name, tags);
    Metric::distribution(context, 1.0)
}

fn distribution_metric_with_origin_tags(
    name: &'static str, tags: &[&'static str], origin_tags: &[&'static str],
) -> Metric {
    let origin_tag_set: TagSet = origin_tags.iter().map(|s| Tag::from(*s)).collect();
    let context = Context::from_static_parts(name, tags).with_origin_tags(origin_tag_set.into_shared());
    Metric::distribution(context, 1.0)
}

fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
    let context = Context::from_static_parts(name, tags);
    Metric::counter(context, 1.0)
}

fn tag_names(metric: &Metric) -> Vec<String> {
    let mut names: Vec<_> = metric
        .context()
        .tags()
        .into_iter()
        .map(|t| t.as_str().to_owned())
        .collect();
    names.sort();
    names
}

fn origin_tag_names(metric: &Metric) -> Vec<String> {
    let mut names: Vec<_> = metric
        .context()
        .origin_tags()
        .into_iter()
        .map(|t| t.as_str().to_owned())
        .collect();
    names.sort();
    names
}

fn tag_rule(metric_name: &str, action: TagFilterAction, tags: &[&str]) -> TagFilterRule {
    TagFilterRule {
        metric_name: metric_name.to_string(),
        action,
        tags: tags.iter().map(|tag| tag.to_string()).collect(),
    }
}

fn value_rule(metric_prefix: &str, tag_name: &str, values: &[&str], on_miss: TagValueMismatch) -> TagValueRule {
    TagValueRule {
        metric_prefix: metric_prefix.to_string(),
        tag_name: tag_name.to_string(),
        values: values.iter().map(|value| (*value).to_string()).collect(),
        on_miss,
    }
}

fn replace(replacement: &str) -> TagValueMismatch {
    TagValueMismatch::Replace(replacement.to_string())
}

/// Compiles tag rules alone.
fn compile_filters(rules: &[TagFilterRule]) -> TagProjectionRules {
    TagProjectionRules::new(rules, ValueRules::default())
}

/// Compiles tag rules and value rules.
fn compile_all_filters(
    rules: &[TagFilterRule], value_rules: &[TagValueRule],
) -> Result<TagProjectionRules, InvalidTagValueRules> {
    Ok(TagProjectionRules::new(rules, ValueRules::new(value_rules)?))
}

fn filter(metric: &mut Metric, rules: &TagProjectionRules) -> FilterMetricTagsOutcome {
    filter_metric_tags(metric, &mut TagSetMutViewState::default(), rules)
}

#[test]
fn filter_metric_tags_treats_distribution_and_count_metrics_identically() {
    // `filter_metric_tags` applies the same rule logic to distribution (sketch) and count metrics. The kind guard
    // (exercised by `the_kernel_rewrites_only_counters_and_distributions`) is what restricts which metric types ever
    // reach this function; the function itself never branches on the type. Each case is asserted against both a
    // distribution and a counter, and against both the resulting tags and the returned outcome, so the two paths
    // can't silently drift apart.
    struct Case {
        name: &'static str,
        action: TagFilterAction,
        filter_metric_name: &'static str,
        filter_tags: &'static [&'static str],
        input_tags: &'static [&'static str],
        expected_tags: &'static [&'static str],
        expected_outcome: FilterMetricTagsOutcome,
    }

    let cases = [
        Case {
            name: "exclude removes the listed tags",
            action: TagFilterAction::Exclude,
            filter_metric_name: "my.metric",
            filter_tags: &["env", "host"],
            input_tags: &["env:prod", "service:web", "host:h1"],
            expected_tags: &["service:web"],
            expected_outcome: FilterMetricTagsOutcome::Modified { removed_tags: 2 },
        },
        Case {
            name: "include keeps only the listed tags",
            action: TagFilterAction::Include,
            filter_metric_name: "my.metric",
            filter_tags: &["env"],
            input_tags: &["env:prod", "service:web", "host:h1"],
            expected_tags: &["env:prod"],
            expected_outcome: FilterMetricTagsOutcome::Modified { removed_tags: 2 },
        },
        Case {
            name: "a rule for a different metric name is a miss",
            action: TagFilterAction::Exclude,
            filter_metric_name: "other.metric",
            filter_tags: &["env"],
            input_tags: &["env:prod", "service:web"],
            expected_tags: &["env:prod", "service:web"],
            expected_outcome: FilterMetricTagsOutcome::RuleMiss,
        },
        Case {
            name: "a matching rule that removes no tags is a no-op",
            action: TagFilterAction::Exclude,
            filter_metric_name: "my.metric",
            filter_tags: &["region"],
            input_tags: &["env:prod", "service:web"],
            expected_tags: &["env:prod", "service:web"],
            expected_outcome: FilterMetricTagsOutcome::NoChange,
        },
    ];

    type MetricBuilder = fn(&'static str, &[&'static str]) -> Metric;
    let builders: [(&str, MetricBuilder); 2] = [("distribution", distribution_metric), ("counter", counter_metric)];

    for case in &cases {
        let filters = compile_filters(&[tag_rule(case.filter_metric_name, case.action, case.filter_tags)]);

        for (kind, build) in builders {
            let mut metric = build("my.metric", case.input_tags);
            let outcome = filter(&mut metric, &filters);

            let expected_tags: Vec<String> = case.expected_tags.iter().map(|s| s.to_string()).collect();
            assert_eq!(tag_names(&metric), expected_tags, "{kind}: {}", case.name);
            assert_eq!(outcome, case.expected_outcome, "{kind}: {}", case.name);
        }
    }
}

#[test]
fn value_allowlist_matches_metric_prefix_and_filters_values() {
    let filters = compile_all_filters(
        &[],
        &[value_rule(
            "my.",
            "customer_id",
            &["top-1", "top-2"],
            TagValueMismatch::Remove,
        )],
    )
    .expect("valid value rules should compile");

    let mut allowed = counter_metric("my.metric", &["customer_id:top-1", "service:web"]);
    assert_eq!(filter(&mut allowed, &filters), FilterMetricTagsOutcome::NoChange);
    assert_eq!(tag_names(&allowed), vec!["customer_id:top-1", "service:web"]);

    let mut unlisted = counter_metric("my.metric", &["customer_id:long-tail", "service:web"]);
    assert_eq!(
        filter(&mut unlisted, &filters),
        FilterMetricTagsOutcome::Modified { removed_tags: 1 }
    );
    assert_eq!(tag_names(&unlisted), vec!["service:web"]);

    let mut unrelated = counter_metric("other.metric", &["customer_id:long-tail", "service:web"]);
    assert_eq!(filter(&mut unrelated, &filters), FilterMetricTagsOutcome::RuleMiss);
    assert_eq!(tag_names(&unrelated), vec!["customer_id:long-tail", "service:web"]);
}

#[test]
fn value_allowlist_replaces_and_deduplicates_unlisted_values() {
    let filters = compile_all_filters(&[], &[value_rule("my.metric", "customer_id", &[], replace("other"))])
        .expect("valid value rules should compile");
    let mut metric = distribution_metric(
        "my.metric",
        &[
            "customer_id:first",
            "customer_id:second",
            "customer_id:other",
            "service:web",
        ],
    );

    assert_eq!(
        filter(&mut metric, &filters),
        FilterMetricTagsOutcome::Modified { removed_tags: 2 }
    );
    assert_eq!(tag_names(&metric), vec!["customer_id:other", "service:web"]);
}

#[test]
fn value_allowlist_ignores_bare_tags_and_treats_empty_strings_as_values() {
    let filters = compile_all_filters(
        &[],
        &[value_rule(
            "my.metric",
            "customer_id",
            &["top-1"],
            TagValueMismatch::Remove,
        )],
    )
    .expect("valid value rules should compile");
    let mut unlisted_empty = counter_metric("my.metric", &["customer_id", "customer_id:", "service:web"]);

    assert_eq!(
        filter(&mut unlisted_empty, &filters),
        FilterMetricTagsOutcome::Modified { removed_tags: 1 }
    );
    assert_eq!(tag_names(&unlisted_empty), vec!["customer_id", "service:web"]);

    let filters = compile_all_filters(
        &[],
        &[value_rule("my.metric", "customer_id", &[""], TagValueMismatch::Remove)],
    )
    .expect("valid value rules should compile");
    let mut listed_empty = counter_metric("my.metric", &["customer_id", "customer_id:", "service:web"]);

    assert_eq!(filter(&mut listed_empty, &filters), FilterMetricTagsOutcome::NoChange);
    assert_eq!(
        tag_names(&listed_empty),
        vec!["customer_id", "customer_id:", "service:web"]
    );
}

#[test]
fn value_allowlist_applies_to_origin_tags() {
    let filters = compile_all_filters(
        &[],
        &[value_rule("my.metric", "customer_id", &["top-1"], replace("other"))],
    )
    .expect("valid value rules should compile");
    let mut metric = distribution_metric_with_origin_tags(
        "my.metric",
        &[],
        &["customer_id:top-1", "customer_id:long-tail", "service:web"],
    );

    assert_eq!(
        filter(&mut metric, &filters),
        FilterMetricTagsOutcome::Modified { removed_tags: 1 }
    );
    assert_eq!(
        origin_tag_names(&metric),
        vec!["customer_id:other", "customer_id:top-1", "service:web"]
    );
}

#[test]
fn invalid_value_rules_do_not_compile() {
    // (value rules, expected error)
    let cases = [
        (
            vec![
                value_rule("my.", "customer_id", &["top-1"], replace("other")),
                value_rule("my.metric", "region", &["us-east-1"], replace("other")),
            ],
            InvalidTagValueRules::OverlappingPrefixes {
                shorter: "my.".to_string(),
                longer: "my.metric".to_string(),
            },
        ),
        (
            vec![
                value_rule("my.", "customer_id", &[], TagValueMismatch::Remove),
                value_rule("my.", "region", &[], TagValueMismatch::Remove),
                value_rule("my.", "customer_id", &["top-2"], replace("other")),
            ],
            InvalidTagValueRules::DuplicateRule {
                metric_prefix: "my.".to_string(),
                tag_name: "customer_id".to_string(),
            },
        ),
        (
            vec![value_rule("", "customer_id", &[], TagValueMismatch::Remove)],
            InvalidTagValueRules::EmptyMetricPrefix { rule: 1 },
        ),
        (
            vec![value_rule("my.", "", &[], TagValueMismatch::Remove)],
            InvalidTagValueRules::EmptyTagName {
                rule: 1,
                metric_prefix: "my.".to_string(),
            },
        ),
        (
            vec![value_rule("my.", "customer:id", &[], TagValueMismatch::Remove)],
            InvalidTagValueRules::TagNameContainsColon {
                rule: 1,
                tag_name: "customer:id".to_string(),
            },
        ),
    ];
    for (rules, expected) in cases {
        assert_eq!(ValueRules::new(&rules).err(), Some(expected));
    }
}

#[test]
fn same_value_allowlist_prefix_for_different_tags_applies_both_rules() {
    let filters = compile_all_filters(
        &[],
        &[
            value_rule("my.", "customer_id", &["top-1"], TagValueMismatch::Remove),
            value_rule("my.", "region", &["us-east-1"], TagValueMismatch::Remove),
        ],
    )
    .expect("different tags may use the same prefix");
    let mut metric = distribution_metric("my.metric.requests", &["customer_id:long-tail", "region:us-west-2"]);

    assert_eq!(
        filter(&mut metric, &filters),
        FilterMetricTagsOutcome::Modified { removed_tags: 2 }
    );
    assert!(tag_names(&metric).is_empty());
}

#[test]
fn value_allowlist_preserves_whitespace_in_configured_strings() {
    let filters = compile_all_filters(
        &[],
        &[value_rule("my. ", "customer_id ", &[" top-1 "], replace(" other "))],
    )
    .expect("valid value rules should compile");

    let mut allowed = distribution_metric("my. requests", &["customer_id : top-1 "]);
    assert_eq!(filter(&mut allowed, &filters), FilterMetricTagsOutcome::NoChange);
    assert_eq!(tag_names(&allowed), vec!["customer_id : top-1 "]);

    let mut replaced = distribution_metric("my. requests", &["customer_id :long-tail"]);
    assert_eq!(
        filter(&mut replaced, &filters),
        FilterMetricTagsOutcome::Modified { removed_tags: 1 }
    );
    assert_eq!(tag_names(&replaced), vec!["customer_id : other "]);
}

#[test]
fn compiled_rule_count_counts_exact_rules_and_each_prefix_tag_rule() {
    let filters = compile_all_filters(
        &[tag_rule("my.metric", TagFilterAction::Exclude, &["host"])],
        &[
            value_rule("my.", "customer_id", &[], TagValueMismatch::Remove),
            value_rule("my.", "region", &[], TagValueMismatch::Remove),
        ],
    )
    .expect("rules should compile");
    assert_eq!(filters.rule_count(), 3);
}

#[test]
fn tag_rules_combine_by_name_and_exclude_wins() {
    // (rules, input tags, expected tags)
    let cases: Vec<(Vec<TagFilterRule>, &[&'static str], &[&str])> = vec![
        // A bare tag's name is the whole tag.
        (
            vec![tag_rule("my.dist", TagFilterAction::Exclude, &["production"])],
            &["production", "service:web"],
            &["service:web"],
        ),
        // `include` with no tags removes every tag; `exclude` with no tags keeps them all.
        (
            vec![tag_rule("my.dist", TagFilterAction::Include, &[])],
            &["env:prod", "service:web"],
            &[],
        ),
        (
            vec![tag_rule("my.dist", TagFilterAction::Exclude, &[])],
            &["env:prod", "service:web"],
            &["env:prod", "service:web"],
        ),
        // Rules with the same action combine.
        (
            vec![
                tag_rule("my.dist", TagFilterAction::Exclude, &["env"]),
                tag_rule("my.dist", TagFilterAction::Exclude, &["host"]),
            ],
            &["env:prod", "host:h1", "service:web"],
            &["service:web"],
        ),
        // Conflicting actions: `exclude` wins, whichever comes first, with its own tags only.
        (
            vec![
                tag_rule("my.dist", TagFilterAction::Include, &["env"]),
                tag_rule("my.dist", TagFilterAction::Exclude, &["host"]),
            ],
            &["env:prod", "host:h1", "service:web"],
            &["env:prod", "service:web"],
        ),
        (
            vec![
                tag_rule("my.dist", TagFilterAction::Exclude, &["host"]),
                tag_rule("my.dist", TagFilterAction::Include, &["env"]),
            ],
            &["env:prod", "host:h1", "service:web"],
            &["env:prod", "service:web"],
        ),
        // No rules, and rules with an empty name, change nothing.
        (vec![], &["env:prod", "service:web"], &["env:prod", "service:web"]),
        (
            vec![tag_rule("", TagFilterAction::Include, &[])],
            &["env:prod", "service:web"],
            &["env:prod", "service:web"],
        ),
    ];
    for (rules, input, expected) in cases {
        let filters = compile_filters(&rules);
        let mut metric = distribution_metric("my.dist", input);
        filter(&mut metric, &filters);
        assert_eq!(tag_names(&metric), expected, "{:?}", rules);
    }
}

#[test]
fn empty_metric_name_is_ignored() {
    let filters = compile_filters(&[
        tag_rule("", TagFilterAction::Exclude, &["env"]),
        tag_rule("my.dist", TagFilterAction::Exclude, &["host"]),
    ]);

    assert!(!filters.key_filters.contains_key(""));
    assert!(filters.key_filters.contains_key("my.dist"));
}

#[test]
fn origin_tags_are_filtered_like_tags() {
    // (action, rule tags, tags, origin tags, expected tags, expected origin tags)
    type Case = (
        TagFilterAction,
        &'static [&'static str],
        &'static [&'static str],
        &'static [&'static str],
        &'static [&'static str],
        &'static [&'static str],
    );
    let cases: [Case; 4] = [
        (
            TagFilterAction::Exclude,
            &["env", "host"],
            &["env:prod"],
            &["env:prod", "host:h1", "service:web"],
            &[],
            &["service:web"],
        ),
        (
            TagFilterAction::Include,
            &["env"],
            &["env:prod"],
            &["env:prod", "host:h1", "service:web"],
            &["env:prod"],
            &["env:prod"],
        ),
        (
            TagFilterAction::Exclude,
            &["env"],
            &["env:prod", "service:web"],
            &[],
            &["service:web"],
            &[],
        ),
        (
            TagFilterAction::Exclude,
            &["env", "host"],
            &["env:prod", "service:web", "host:h1"],
            &["env:prod", "host:h1", "region:us-east-1"],
            &["service:web"],
            &["region:us-east-1"],
        ),
    ];
    for (action, rule_tags, tags, origin_tags, expected_tags, expected_origin_tags) in cases {
        let filters = compile_filters(&[tag_rule("my.dist", action, rule_tags)]);
        let mut metric = distribution_metric_with_origin_tags("my.dist", tags, origin_tags);
        filter(&mut metric, &filters);
        assert_eq!(tag_names(&metric), expected_tags);
        assert_eq!(origin_tag_names(&metric), expected_origin_tags);
    }
}

#[test]
fn filtering_origin_tags_does_not_affect_shared_origin() {
    let origin_tag_set: TagSet = ["env:prod", "host:h1", "service:web"]
        .iter()
        .map(|s| Tag::from(*s))
        .collect();
    let shared_origin = origin_tag_set.into_shared();

    let ctx1 = Context::from_static_parts("my.dist", &[]).with_origin_tags(shared_origin.clone());
    let ctx2 = Context::from_static_parts("my.dist", &[]).with_origin_tags(shared_origin.clone());

    let mut metric1 = Metric::distribution(ctx1, 1.0);
    let metric2 = Metric::distribution(ctx2, 1.0);

    let filters = compile_filters(&[tag_rule("my.dist", TagFilterAction::Exclude, &["env", "host"])]);
    filter(&mut metric1, &filters);

    assert_eq!(origin_tag_names(&metric1), vec!["service:web"]);
    assert_eq!(origin_tag_names(&metric2), vec!["env:prod", "host:h1", "service:web"]);
}

#[test]
fn modified_filter_marks_tagsets_as_modified() {
    let filters = compile_filters(&[tag_rule("my.dist", TagFilterAction::Exclude, &["host"])]);
    let mut metric = distribution_metric("my.dist", &["env:prod", "host:h1"]);

    let outcome = filter(&mut metric, &filters);

    assert_eq!(outcome, FilterMetricTagsOutcome::Modified { removed_tags: 1 });
    assert_eq!(tag_names(&metric), vec!["env:prod"]);
    assert!(metric.context().tags().is_modified());
}

#[test]
fn no_change_does_not_mark_tagsets_as_modified() {
    let filters = compile_filters(&[tag_rule("my.dist", TagFilterAction::Exclude, &["region"])]);
    let shared_tags = ["env:prod", "host:h1"]
        .into_iter()
        .map(Tag::from)
        .collect::<TagSet>()
        .into_shared();
    let context = Context::from_parts("my.dist", shared_tags);
    let mut metric = Metric::distribution(context, 1.0);

    assert!(!metric.context().tags().is_modified());
    assert!(!metric.context().origin_tags().is_modified());

    let outcome = filter(&mut metric, &filters);

    assert_eq!(outcome, FilterMetricTagsOutcome::NoChange);
    assert_eq!(tag_names(&metric), vec!["env:prod", "host:h1"]);
    assert!(!metric.context().tags().is_modified());
    assert!(!metric.context().origin_tags().is_modified());
}

#[test]
fn telemetry_records_hits_misses_filtered_tags_size_and_updates() {
    let recorder = TestRecorder::default();
    let _local = set_default_local_recorder(&recorder);

    let mut telemetry = TagProjectionTelemetry::new(&MetricsBuilder::default());

    for name in [
        "tag_filterlist_rule_hits_total",
        "tag_filterlist_rule_misses_total",
        "tag_filterlist_noop_hits_total",
        "tag_filterlist_metrics_modified_total",
        "tag_filterlist_tags_filtered_total",
        "tag_filterlist_updates_total",
    ] {
        assert_eq!(recorder.counter(name), Some(0), "{}", name);
    }

    telemetry.record(FilterMetricTagsOutcome::RuleMiss);
    telemetry.record(FilterMetricTagsOutcome::NoChange);
    telemetry.record(FilterMetricTagsOutcome::Modified { removed_tags: 3 });

    assert_eq!(recorder.counter("tag_filterlist_rule_hits_total"), Some(2));
    assert_eq!(recorder.counter("tag_filterlist_rule_misses_total"), Some(1));
    assert_eq!(recorder.counter("tag_filterlist_noop_hits_total"), Some(1));
    assert_eq!(recorder.counter("tag_filterlist_metrics_modified_total"), Some(1));
    assert_eq!(recorder.counter("tag_filterlist_tags_filtered_total"), Some(3));

    for size in [5, 3, 0] {
        telemetry.set_size(size);
        assert_eq!(recorder.gauge("tag_filterlist_size"), Some(size as f64));
    }

    telemetry.increment_updates();
    telemetry.increment_updates();
    assert_eq!(recorder.counter("tag_filterlist_updates_total"), Some(2));
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the test runtime builds")
}

#[test]
fn the_kernel_rewrites_only_counters_and_distributions() {
    let runtime = runtime();
    let _context = runtime.enter();
    let recorder = TestRecorder::default();
    let _local = set_default_local_recorder(&recorder);

    let rules = compile_all_filters(
        &[tag_rule("svc.latency", TagFilterAction::Exclude, &["host"])],
        &[value_rule("svc.", "customer_id", &[], replace("other"))],
    )
    .unwrap();
    let mut kernel = TagProjection::new(
        rules,
        Some(ContextMemo::new(NATIVE_CONTEXT_CACHE_ID, 100)),
        TagProjectionTelemetry::new(&MetricsBuilder::default()),
    );
    assert_eq!(recorder.gauge("tag_filterlist_size"), Some(2.0));

    // Every kind with a context the rules name, and a counter no rule names.
    let tags = &["host:h1", "env:prod", "customer_id:long-tail"];
    let context = || Context::from_static_parts("svc.latency", tags);
    let input = buffer_of([
        Metric::distribution(context(), 1.0),
        Metric::counter(context(), 1.0),
        Metric::gauge(context(), 1.0),
        Metric::rate(context(), 1.0, std::time::Duration::from_secs(10)),
        Metric::set(context(), "member"),
        Metric::histogram(context(), 1.0),
        Metric::counter(Context::from_static_parts("other", tags), 1.0),
    ]);
    let consumed = consume(&mut kernel, &BatchHeader::default(), 1, input);

    let projected = ["customer_id:other", "env:prod"];
    let unchanged = ["customer_id:long-tail", "env:prod", "host:h1"];
    let tags = consumed.ports[0].iter().map(tag_names).collect::<Vec<_>>();
    assert_eq!(
        tags,
        [
            &projected[..],
            &projected,
            &unchanged,
            &unchanged,
            &unchanged,
            &unchanged,
            &unchanged
        ]
    );
    assert!(is_projected(&MetricValues::counter(1.0)));
    assert!(!is_projected(&MetricValues::gauge(1.0)));

    // Two projected metrics, the second served from the memo, and the uncovered counter, a miss.
    assert_eq!(recorder.counter("tag_filterlist_rule_hits_total"), Some(2));
    assert_eq!(recorder.counter("tag_filterlist_metrics_modified_total"), Some(2));
    assert_eq!(recorder.counter("tag_filterlist_tags_filtered_total"), Some(4));
    assert_eq!(recorder.counter("tag_filterlist_rule_misses_total"), Some(1));
}

#[test]
fn the_memo_serves_the_result_computed_for_a_context() {
    let runtime = runtime();
    let _context = runtime.enter();

    let rules = || compile_filters(&[tag_rule("svc.latency", TagFilterAction::Exclude, &["host"])]);
    let mut kernel = TagProjection::new(
        rules(),
        Some(ContextMemo::new("test/context_cache", 0)),
        TagProjectionTelemetry::noop(),
    );

    // A memo with a capacity of zero still holds one context.
    let mut first = counter_metric("svc.latency", &["host:h1", "env:prod"]);
    assert_eq!(
        kernel.apply(&mut first),
        Some(FilterMetricTagsOutcome::Modified { removed_tags: 1 })
    );
    let memo = kernel.memo.as_ref().unwrap();
    let original = Context::from_static_parts("svc.latency", &["host:h1", "env:prod"]);
    assert_eq!(memo.cache.get(&original), Some(Some((first.context().clone(), 1))));

    // The memoized context replaces the context of a metric with an equal one.
    let mut second = counter_metric("svc.latency", &["host:h1", "env:prod"]);
    assert_eq!(
        kernel.apply(&mut second),
        Some(FilterMetricTagsOutcome::Modified { removed_tags: 1 })
    );
    assert_eq!(tag_names(&second), vec!["env:prod"]);

    // Misses are not memoized.
    let mut other = counter_metric("other", &["host:h1"]);
    assert_eq!(kernel.apply(&mut other), Some(FilterMetricTagsOutcome::RuleMiss));
    let memo = kernel.memo.as_ref().unwrap();
    assert_eq!(memo.cache.get(&other.context().clone()), None);

    // New rules come with a new memo, so no result of the old rules survives.
    kernel.replace_rules(compile_filters(&[]), Some(ContextMemo::new("test/context_cache", 10)));
    let mut third = counter_metric("svc.latency", &["host:h1", "env:prod"]);
    assert_eq!(kernel.apply(&mut third), Some(FilterMetricTagsOutcome::RuleMiss));
    assert_eq!(tag_names(&third), vec!["env:prod", "host:h1"]);
}

/// Tag rules over the names and tag keys metrics have.
fn arb_tag_rules() -> impl Strategy<Value = Vec<reference::MetricTagFilterEntry>> {
    let rule = (
        select(NAME_VOCABULARY),
        any::<bool>(),
        vec(select(TAG_KEY_VOCABULARY), 0..3),
    )
        .prop_map(|(name, exclude, tags)| reference::MetricTagFilterEntry {
            metric_name: name.to_string(),
            action: if exclude {
                reference::FilterAction::Exclude
            } else {
                reference::FilterAction::Include
            },
            tags: tags.into_iter().map(str::to_string).collect(),
        });
    vec(rule, 0..4)
}

/// Value rules over the prefixes and tags metrics have, including invalid sets: overlapping prefixes, repeated rules,
/// empty prefixes and tag names, and tag names with a `:`.
fn arb_value_rules() -> impl Strategy<Value = Vec<reference::MetricTagValueAllowlistEntry>> {
    let rule = (
        select(PREFIX_VOCABULARY),
        prop_oneof![select(TAG_KEY_VOCABULARY), Just("env:prod")],
        vec(select(TAG_VALUE_VOCABULARY), 0..3),
        any::<bool>(),
        select(vec!["other", "", "prod", " spaced "]),
    )
        .prop_map(
            |(prefix, tag, values, replace, replacement)| reference::MetricTagValueAllowlistEntry {
                metric_prefix: prefix.to_string(),
                tag_name: tag.to_string(),
                values: values.into_iter().map(str::to_string).collect(),
                on_miss: if replace {
                    reference::TagValueMismatchAction::Replace
                } else {
                    reference::TagValueMismatchAction::Remove
                },
                replacement: replacement.to_string(),
            },
        );
    vec(rule, 0..3)
}

/// Converts the native configuration entries to the kernel's parameters, as the legacy component does.
fn to_params(
    rules: &[reference::MetricTagFilterEntry], value_rules: &[reference::MetricTagValueAllowlistEntry],
) -> (Vec<TagFilterRule>, Vec<TagValueRule>) {
    let rules = rules
        .iter()
        .map(|rule| TagFilterRule {
            metric_name: rule.metric_name.clone(),
            action: match rule.action {
                reference::FilterAction::Include => TagFilterAction::Include,
                reference::FilterAction::Exclude => TagFilterAction::Exclude,
            },
            tags: rule.tags.clone(),
        })
        .collect();
    let value_rules = value_rules
        .iter()
        .map(|rule| TagValueRule {
            metric_prefix: rule.metric_prefix.clone(),
            tag_name: rule.tag_name.clone(),
            values: rule.values.clone(),
            on_miss: match rule.on_miss {
                reference::TagValueMismatchAction::Remove => TagValueMismatch::Remove,
                reference::TagValueMismatchAction::Replace => TagValueMismatch::Replace(rule.replacement.clone()),
            },
        })
        .collect();
    (rules, value_rules)
}

/// Metrics whose names and tags the generated rules name often: generated metrics, with some of their tags replaced
/// by tags from the vocabularies, repeated so that the memo serves some of them.
fn arb_metrics() -> impl Strategy<Value = Vec<Metric>> {
    let metric = (arb_metric(), select(NAME_VOCABULARY), any::<bool>());
    vec(metric, 1..16)
        .prop_map(|metrics| {
            metrics
                .into_iter()
                .map(|(metric, name, rename)| {
                    if rename {
                        let (context, values, metadata) = metric.into_parts();
                        Metric::from_parts(context.with_name(name), values, metadata)
                    } else {
                        metric
                    }
                })
                .collect::<Vec<_>>()
        })
        .prop_flat_map(|metrics| {
            let count = metrics.len();
            (Just(metrics), vec(0..count, 0..8))
        })
        .prop_map(|(mut metrics, repeats)| {
            for index in repeats {
                metrics.push(metrics[index].clone());
            }
            metrics
        })
}

const COUNTERS: [&str; 6] = [
    "tag_filterlist_rule_hits_total",
    "tag_filterlist_rule_misses_total",
    "tag_filterlist_noop_hits_total",
    "tag_filterlist_metrics_modified_total",
    "tag_filterlist_tags_filtered_total",
    "tag_filterlist_size",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_projection_matches_the_pre_extraction_transform(
        entries in arb_tag_rules(),
        value_entries in arb_value_rules(),
        capacity in prop_oneof![Just(0usize), Just(2usize), Just(1000usize)],
        batches in vec(arb_metrics(), 1..3),
    ) {
        let runtime = runtime();
        let _context = runtime.enter();
        let (rules, value_rules) = to_params(&entries, &value_entries);

        // The rules compile, or fail to, alike.
        let expected = reference::compile_all_filters(&[], &value_entries);
        let actual = ValueRules::new(&value_rules);
        let (expected_values, actual_values) = match (expected, actual) {
            (Ok(expected), Ok(actual)) => (expected.value_prefix_filters, actual),
            (Err(_), Err(_)) => return Ok(()),
            (expected, actual) => {
                return Err(TestCaseError::fail(format!(
                    "the frozen rules compiled: {}, the kernel's: {}",
                    expected.is_ok(),
                    actual.is_ok()
                )));
            }
        };

        let expected_recorder = TestRecorder::default();
        let actual_recorder = TestRecorder::default();
        let mut expected = {
            let _local = set_default_local_recorder(&expected_recorder);
            let telemetry = reference::Telemetry::new(&MetricsBuilder::default());
            let filters = reference::compile_filters_with_values(&entries, expected_values);
            telemetry.set_size(filters.rule_count());
            reference::TagFilterlist {
                filters,
                telemetry,
                context_cache: reference::build_context_cache(capacity),
            }
        };
        let mut actual = {
            let _local = set_default_local_recorder(&actual_recorder);
            TagProjection::new(
                TagProjectionRules::new(&rules, actual_values),
                Some(ContextMemo::new(NATIVE_CONTEXT_CACHE_ID, capacity)),
                TagProjectionTelemetry::new(&MetricsBuilder::default()),
            )
        };

        // Batches in order, through the native run loop and the kernel: the same metrics, in the same order, with
        // the same tags in the same order.
        let mut view_state = TagSetMutViewState::default();
        for batch in batches {
            let mut native = buffer_of(batch.iter().cloned());
            expected.process_buffer(&mut native, &mut view_state);
            let consumed = consume(&mut actual, &BatchHeader::default(), 1, buffer_of(batch.iter().cloned()));
            let native = crate::test_support::metrics_of(native);
            prop_assert_eq!(native.len(), consumed.ports[0].len());
            for (native, kernel) in native.iter().zip(&consumed.ports[0]) {
                prop_assert_eq!(describe(native), describe(kernel));
                prop_assert_eq!(native.context(), kernel.context());
            }
        }

        // The same telemetry.
        for name in &COUNTERS[..5] {
            prop_assert_eq!(expected_recorder.counter(*name), actual_recorder.counter(*name), "{}", name);
        }
        prop_assert_eq!(expected_recorder.gauge(COUNTERS[5]), actual_recorder.gauge(COUNTERS[5]));
    }
}
